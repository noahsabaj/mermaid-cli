//! Atomic file writes.
//!
//! Plain `fs::write` truncates the target to zero length and then writes the
//! new contents in place. A crash / kill / disk-full between those two steps
//! leaves the file empty or half-written — catastrophic for session,
//! checkpoint, and plugin-lockfile state that is rewritten in full on every
//! save. [`write_atomic`] writes to a temp sibling, fsyncs it, then renames over
//! the target, so a reader always sees either the old complete file or the new
//! complete file.

use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A sibling temp file untouched for at least this long is treated as orphaned by
/// a crashed writer and swept on the next write to the same target. The window is
/// deliberately generous: `write_atomic` rewrites small session/checkpoint/
/// lockfile state in a single pass, so a legitimate in-flight temp (ours or
/// another live writer's) is always far younger than this and is never collected.
const STALE_TEMP_SECS: u64 = 3600;

/// Write `bytes` to `path` atomically: temp file in the same directory →
/// `sync_all` → rename over the destination. The rename is atomic on the same
/// filesystem (and replaces an existing target on both Unix and Windows).
///
/// # Errors
///
/// Creating the parent directory, creating or writing the temp sibling,
/// `sync_all`, and the rename. On a rename failure the temp file is removed
/// and the destination is left exactly as it was — a failed call never leaves
/// a truncated target, which is the whole point of the helper.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    write_atomic_inner(path, bytes, None)
}

/// Like [`write_atomic`], but create the temp file owner-only so the renamed
/// destination is never even briefly readable by another user. On Unix that
/// is the given permission `mode` (e.g. `0o600`); on Windows, where there is
/// no mode, it is a protected DACL granting the owning user and `SYSTEM` and
/// no one else -- the same descriptor the daemon puts on its pipe. Any other
/// platform creates the file at the default permissions. Use this for
/// secret-bearing files such as the config.
///
/// # Errors
///
/// Exactly [`write_atomic`]'s, plus the `mode` being rejected when the temp
/// file is created on Unix, or the security descriptor failing to build on
/// Windows.
pub fn write_atomic_with_mode(path: &Path, bytes: &[u8], mode: u32) -> std::io::Result<()> {
    write_atomic_inner(path, bytes, Some(mode))
}

fn write_atomic_inner(path: &Path, bytes: &[u8], mode: Option<u32>) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;

    let stem = path.file_name().and_then(|n| n.to_str()).unwrap_or("tmp");

    // Best-effort: clear temp siblings stranded by a previous crashed write to
    // this same target. A crash between the temp create and `rename` below leaves
    // `.<stem>.<pid>.<n>.tmp` behind forever (cleanup otherwise runs only on
    // rename-error or success), so without this repeated crashes would litter the
    // directory. The sweep only removes clearly abandoned (stale) temps and never
    // the destination or a fresh/in-flight temp.
    sweep_stale_temps(parent, stem, Duration::from_secs(STALE_TEMP_SECS));

    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(".{}.{}.{}.tmp", stem, std::process::id(), n));

    {
        let mut f = create_temp(&tmp, mode)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }

    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }

    // Best-effort durability of the rename itself. Opening a directory as a
    // File is not supported on Windows, so this is a silent no-op there.
    if let Ok(dir) = File::open(parent) {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// Create the temp file, honoring an explicit Unix `mode` when given so a secret
/// file is written 0600 from the start rather than at the process umask.
#[cfg(unix)]
fn create_temp(tmp: &Path, mode: Option<u32>) -> std::io::Result<File> {
    match mode {
        Some(mode) => {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(mode)
                .open(tmp)
        },
        None => File::create(tmp),
    }
}

/// Create the temp file with an owner-only DACL when a mode was asked for:
/// Windows has no permission bits, so "0600" means a protected descriptor
/// naming the owning user and `SYSTEM`. `CreateFileW` takes the descriptor at
/// creation, so there is no window in which the file carries the directory's
/// inherited ACL; the rename that follows keeps the object's own descriptor.
#[cfg(windows)]
fn create_temp(tmp: &Path, mode: Option<u32>) -> std::io::Result<File> {
    if mode.is_none() {
        return File::create(tmp);
    }
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{
        CREATE_ALWAYS, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_WRITE,
    };

    let mut security = crate::daemon::PipeSecurity::owner_only().map_err(std::io::Error::other)?;
    let wide: Vec<u16> = tmp
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: `wide` is null-terminated and outlives the call; the security
    // attributes point at a descriptor that `security` keeps alive until it
    // drops, after the handle exists.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_GENERIC_WRITE,
            0,
            security.attributes_ptr().cast(),
            CREATE_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: a freshly created, owned file handle that nothing else closes.
    Ok(unsafe { File::from_raw_handle(handle.cast()) })
}

#[cfg(not(any(unix, windows)))]
fn create_temp(tmp: &Path, _mode: Option<u32>) -> std::io::Result<File> {
    File::create(tmp)
}

/// Best-effort sweep of orphaned temp siblings for the target named `stem` in
/// `parent`, left behind when a writer crashed between creating the temp and
/// renaming it over the destination. Only files matching THIS target's temp
/// pattern (`.{stem}.{pid}.{n}.tmp`) and older than `max_age` are removed.
///
/// Safety: the destination is named exactly `stem`, which can never start with
/// the dotted `.{stem}.` prefix, so it is structurally unmatched; and a live,
/// in-flight temp (ours or another concurrent writer's) is younger than
/// `max_age` and so is never collected. Every error is swallowed — a sweep
/// failure must not fail the write.
fn sweep_stale_temps(parent: &Path, stem: &str, max_age: Duration) {
    let prefix = format!(".{stem}.");
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with(&prefix) || !name.ends_with(".tmp") {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|mtime| now.duration_since(mtime).ok())
            .map(|age| age >= max_age)
            .unwrap_or(false);
        if stale {
            let _ = fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_replaces_existing_and_no_temp_left() {
        let dir = std::env::temp_dir().join(format!("mermaid_atomic_{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let target = dir.join("conv.json");
        write_atomic(&target, b"first").unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "first");
        write_atomic(&target, b"second").unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "second");
        // No leftover temp files.
        let leftovers = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn sweep_removes_only_matching_stale_temps() {
        let dir = std::env::temp_dir().join(format!("mermaid_atomic_sweep_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);

        let target = dir.join("conv.json");
        write_atomic(&target, b"live").unwrap();

        // An orphaned temp for THIS target (a crashed prior write).
        let orphan = dir.join(".conv.json.99999.0.tmp");
        fs::write(&orphan, b"half-written").unwrap();
        // A temp for a DIFFERENT target — must be left alone.
        let other = dir.join(".other.json.99999.0.tmp");
        fs::write(&other, b"someone else").unwrap();
        // An unrelated regular file — must be left alone.
        let unrelated = dir.join("notes.txt");
        fs::write(&unrelated, b"keep me").unwrap();

        // max_age = ZERO ⇒ any matching temp qualifies as stale.
        sweep_stale_temps(&dir, "conv.json", Duration::ZERO);

        assert!(!orphan.exists(), "matching stale temp must be swept");
        assert!(other.exists(), "a different target's temp must survive");
        assert!(unrelated.exists(), "unrelated files must survive");
        assert!(target.exists(), "the destination must never be swept");
        assert_eq!(fs::read_to_string(&target).unwrap(), "live");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn sweep_preserves_fresh_in_flight_temps() {
        let dir = std::env::temp_dir().join(format!("mermaid_atomic_fresh_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);

        // A freshly created temp stands in for a concurrent, in-flight write.
        let fresh = dir.join(".conv.json.12345.7.tmp");
        fs::write(&fresh, b"being written").unwrap();

        // A long window must never collect a just-created temp.
        sweep_stale_temps(&dir, "conv.json", Duration::from_secs(STALE_TEMP_SECS));

        assert!(fresh.exists(), "a fresh/in-flight temp must not be swept");
        let _ = fs::remove_dir_all(&dir);
    }

    /// The written file's DACL is exactly the daemon pipe's: protected (no
    /// inherited ACEs), the owning user and `SYSTEM`, nothing else. Both
    /// descriptors are rendered by the same SDDL normaliser before comparing,
    /// because it abbreviates well-known accounts (the CI runner's user is the
    /// built-in Administrator and renders as `LA`, not as its `S-1-5-21-…`).
    #[test]
    #[cfg(windows)]
    fn write_atomic_with_mode_gives_the_file_an_owner_only_dacl() {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Authorization::{
            ConvertSecurityDescriptorToStringSecurityDescriptorW,
            ConvertStringSecurityDescriptorToSecurityDescriptorW, GetNamedSecurityInfoW,
            SDDL_REVISION_1, SE_FILE_OBJECT,
        };
        use windows_sys::Win32::Security::{DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR};

        /// `D:...` for a descriptor's DACL, as Windows itself spells it.
        fn dacl_sddl(descriptor: PSECURITY_DESCRIPTOR) -> String {
            let mut sddl_w: *mut u16 = std::ptr::null_mut();
            let ok = unsafe {
                ConvertSecurityDescriptorToStringSecurityDescriptorW(
                    descriptor,
                    SDDL_REVISION_1,
                    DACL_SECURITY_INFORMATION,
                    &mut sddl_w,
                    std::ptr::null_mut(),
                )
            };
            assert_ne!(
                ok, 0,
                "ConvertSecurityDescriptorToStringSecurityDescriptorW"
            );
            let mut len = 0;
            while unsafe { *sddl_w.add(len) } != 0 {
                len += 1;
            }
            let sddl = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(sddl_w, len) });
            unsafe {
                LocalFree(sddl_w.cast());
            }
            sddl
        }

        let dir = std::env::temp_dir().join(format!("mermaid-atomic-dacl-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("config.toml");
        write_atomic_with_mode(&target, b"secret = true", 0o600).unwrap();

        let wide: Vec<u16> = target
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut actual: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        let rc = unsafe {
            GetNamedSecurityInfoW(
                wide.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut actual,
            )
        };
        assert_eq!(rc, 0, "GetNamedSecurityInfoW");
        let actual_sddl = dacl_sddl(actual);
        unsafe {
            LocalFree(actual.cast());
        }
        let _ = fs::remove_dir_all(&dir);

        let expected_source = crate::daemon::pipe_sddl(&crate::daemon::current_user_sid().unwrap());
        let wide: Vec<u16> = expected_source
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let mut expected: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                SDDL_REVISION_1,
                &mut expected,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(
            ok, 0,
            "ConvertStringSecurityDescriptorToSecurityDescriptorW"
        );
        let expected_sddl = dacl_sddl(expected);
        unsafe {
            LocalFree(expected.cast());
        }
        // The descriptor grants `GA` (generic all); the kernel maps generic
        // rights to the object's specific mask when a FILE is created, so the
        // file reads back with `FA` (file all access). Same grant, file spelling.
        let expected_sddl = expected_sddl.replace(";GA;", ";FA;");

        assert!(
            actual_sddl.starts_with("D:P"),
            "not protected: {actual_sddl}"
        );
        assert_eq!(
            actual_sddl, expected_sddl,
            "file DACL differs from the owner-only descriptor"
        );
    }

    #[test]
    #[cfg(unix)]
    fn write_atomic_with_mode_creates_0600_and_leaves_no_temp() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("mermaid_atomic_mode_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
        let target = dir.join("config.toml");

        write_atomic_with_mode(&target, b"secret = true", 0o600).unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "secret = true");
        let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "config must be created 0600, not at umask");

        // Overwriting a world-readable pre-existing file re-creates it 0600 (the
        // renamed 0600 temp replaces the old file).
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
        write_atomic_with_mode(&target, b"secret = false", 0o600).unwrap();
        let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "an overwrite must not leave the file world-readable"
        );

        let leftovers = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0, "no temp left behind");

        let _ = fs::remove_dir_all(&dir);
    }
}
