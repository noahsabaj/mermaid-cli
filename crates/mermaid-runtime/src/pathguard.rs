//! Lexical path containment for the runtime crate.
//!
//! Resolves a caller- or manifest-supplied path against a trusted root,
//! collapsing `.`/`..` *without* touching the filesystem, and rejects anything
//! that escapes the root. Used by approval replay ([`crate::approval`]) and
//! checkpoint restore ([`crate::checkpoint`]) to confine writes/deletes whose
//! paths come from stored (and therefore potentially tampered) state.
//!
//! This is the runtime-crate sibling of the main crate's `path_safety` helper;
//! `mermaid-runtime` is the lower crate and cannot depend on it.

use std::fs::File;
use std::io;
use std::path::{Component, Path, PathBuf};

use anyhow::Result;

/// Resolve `raw` against `root` and confirm it stays inside `root`.
///
/// `raw` may be relative (joined onto `root`) or absolute (taken as-is). Both
/// the candidate and the root are normalized lexically — `.` is dropped and
/// `..` pops the previous component — so traversal is resolved without symlink
/// expansion or filesystem access (the target may not exist yet, as on a fresh
/// checkout). Returns the normalized in-root path, or `Err` if it escapes.
pub(crate) fn contain_within(root: &Path, raw: &str) -> Result<PathBuf> {
    let candidate = if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        root.join(raw)
    };
    let lexical = normalize_lexical(&candidate);
    let root = normalize_lexical(root);
    anyhow::ensure!(
        lexical.starts_with(&root),
        "path escapes the project root: {raw}"
    );
    Ok(lexical)
}

/// Like [`contain_within`], but also defeats a symlink planted *inside* the
/// root that would redirect a write/delete outside it: the canonical form of
/// the target's nearest existing ancestor must stay within the canonical root.
///
/// The target itself may not exist yet (restoring a deleted file), so we walk
/// up to the nearest existing ancestor and canonicalize that. Falls back to the
/// lexical result when the root doesn't yet exist on disk (nothing to escape
/// through).
pub(crate) fn contain_within_canonical(root: &Path, raw: &str) -> Result<PathBuf> {
    let lexical = contain_within(root, raw)?;
    let canon_root = match std::fs::canonicalize(root) {
        Ok(r) => r,
        Err(_) => return Ok(lexical),
    };
    let mut ancestor = lexical.as_path();
    let existing = loop {
        if ancestor.exists() {
            break Some(ancestor);
        }
        match ancestor.parent() {
            Some(p) => ancestor = p,
            None => break None,
        }
    };
    if let Some(existing) = existing
        && let Ok(canon) = std::fs::canonicalize(existing)
    {
        anyhow::ensure!(
            canon.starts_with(&canon_root),
            "path escapes the project root via a symlink: {raw}"
        );
    }
    Ok(lexical)
}

/// Root-relative, lexically-normalized form of `raw`, erroring if it escapes
/// `root`. This is the `rel` argument for the confined `*_beneath` helpers:
/// they resolve it under a directory fd for `root` with `RESOLVE_BENEATH`, so a
/// swapped-in symlink can't redirect the operation outside `root`. Used by
/// [`crate::approval`] so the replay path gets the same symlink-safe confinement
/// the live tool path has, instead of by-path `std::fs` that follows symlinks.
pub(crate) fn relative_within(root: &Path, raw: &str) -> Result<PathBuf> {
    let abs = contain_within(root, raw)?;
    let root_norm = normalize_lexical(root);
    let rel = abs
        .strip_prefix(&root_norm)
        .map_err(|_| anyhow::anyhow!("path escapes the project root: {raw}"))?;
    Ok(rel.to_path_buf())
}

/// Collapse `.` and `..` components lexically (no filesystem access, no symlink
/// resolution).
fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {},
            Component::ParentDir => {
                out.pop();
            },
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// What kind of access an [`open_beneath`] call needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenIntent {
    /// Open an existing file read-only.
    Read,
    /// Create-or-truncate for writing (`O_WRONLY|O_CREAT|O_TRUNC`).
    WriteTruncate,
}

/// Open `root`-relative `rel` for `intent` **without ever traversing out of
/// `root`** — closing the check-then-write TOCTOU where an intermediate
/// directory is swapped for a symlink after a lexical path check but before the
/// operation (#77). The returned [`File`] is bound to the exact inode the
/// confinement resolved, so subsequent reads/writes can't be redirected.
///
/// On Linux this is enforced atomically by the kernel via
/// `openat2(RESOLVE_BENEATH)` relative to a directory fd for `root`. We use
/// `RESOLVE_BENEATH` (which forbids escaping the root via absolute paths, `..`,
/// or magic links) but **not** `RESOLVE_NO_SYMLINKS`, so legitimate *in-tree*
/// symlinks keep working — only escapes are refused, matching the existing
/// [`contain_within_canonical`] semantics. On pre-5.6 kernels (`ENOSYS`) or
/// non-Linux targets it falls back to `contain_within_canonical` + a plain open
/// (documented best-effort: the lexical check still holds, but the open is by
/// path).
pub fn open_beneath(root: &Path, rel: &Path, intent: OpenIntent) -> io::Result<File> {
    #[cfg(target_os = "linux")]
    {
        match linux::open_beneath(root, rel, intent) {
            Err(e) if e == rustix::io::Errno::NOSYS => {}, // pre-5.6 kernel: fall through
            other => return other.map_err(io::Error::from),
        }
    }
    fallback::open(root, rel, intent)
}

/// Confined `mkdir -p` for a `root`-relative directory path: creates each
/// missing component beneath `root`, refusing to descend through a symlink that
/// escapes (#77). On Linux: `mkdirat` + `openat2(RESOLVE_BENEATH)` per
/// component; otherwise the `contain_within_canonical` + `std::fs` fallback.
pub fn create_dir_all_beneath(root: &Path, rel: &Path) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        match linux::create_dir_all_beneath(root, rel) {
            Err(e) if e == rustix::io::Errno::NOSYS => {},
            other => return other.map_err(io::Error::from),
        }
    }
    fallback::create_dir_all(root, rel)
}

/// Confined unlink of a `root`-relative file (#77): opens the parent under
/// `RESOLVE_BENEATH` and `unlinkat`s the leaf, so a swapped-in symlink parent
/// can't redirect the delete outside `root`.
pub fn remove_file_beneath(root: &Path, rel: &Path) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        match linux::remove_file_beneath(root, rel) {
            Err(e) if e == rustix::io::Errno::NOSYS => {},
            other => return other.map_err(io::Error::from),
        }
    }
    fallback::remove_file(root, rel)
}

/// Atomically write `bytes` to `root`-relative `rel` without ever traversing out
/// of `root`. The temp file is created and `renameat`-swapped beneath the *same*
/// confined directory fd as the destination (Linux `openat2(RESOLVE_BENEATH)`),
/// so a parent dir swapped for an escaping symlink after a path check can't
/// redirect the write (#77), AND a crash/kill/disk-full mid-write leaves the
/// previous file intact instead of a truncated one — `open_beneath` +
/// `WriteTruncate` gives the first guarantee but not the second. Falls back to
/// `contain_within_canonical` + by-path [`crate::write_atomic`] on non-Linux /
/// pre-`openat2` kernels (the same best-effort posture as the other helpers).
pub fn write_atomic_beneath(root: &Path, rel: &Path, bytes: &[u8]) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        match linux::write_atomic_beneath(root, rel, bytes) {
            Err(e) if e == rustix::io::Errno::NOSYS => {},
            other => return other.map_err(io::Error::from),
        }
    }
    fallback::write_atomic(root, rel, bytes)
}

/// Linux `openat2(RESOLVE_BENEATH)` implementation. Each fn returns
/// `Err(Errno::NOSYS)` on a kernel that predates `openat2` (5.6) so the public
/// wrapper can fall back.
#[cfg(target_os = "linux")]
mod linux {
    use std::fs::File;
    use std::io::Write;
    use std::path::{Component, Path};
    use std::sync::atomic::{AtomicU64, Ordering};

    use rustix::fd::OwnedFd;
    use rustix::fs::{
        AtFlags, Mode, OFlags, ResolveFlags, fchmod, mkdirat, open, openat2, renameat, statat,
        unlinkat,
    };
    use rustix::io::Errno;

    use super::OpenIntent;

    /// Open a path-only (`O_PATH`) directory fd for `dir`, used as the anchor
    /// for the confined `openat2` calls below.
    fn open_dir(dir: &Path) -> Result<OwnedFd, Errno> {
        open(
            dir,
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
    }

    /// Descend into `name` beneath `dir_fd`, refusing any escape.
    fn open_subdir(dir_fd: &OwnedFd, name: &Path) -> Result<OwnedFd, Errno> {
        openat2(
            dir_fd,
            name,
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::BENEATH,
        )
    }

    pub(super) fn open_beneath(root: &Path, rel: &Path, intent: OpenIntent) -> Result<File, Errno> {
        let root_fd = open_dir(root)?;
        let (flags, mode) = match intent {
            OpenIntent::Read => (OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty()),
            OpenIntent::WriteTruncate => (
                OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o644),
            ),
        };
        let fd = openat2(&root_fd, rel, flags, mode, ResolveFlags::BENEATH)?;
        Ok(File::from(fd))
    }

    pub(super) fn create_dir_all_beneath(root: &Path, rel: &Path) -> Result<(), Errno> {
        let mut dir = open_dir(root)?;
        for comp in rel.components() {
            let name: &Path = match comp {
                Component::Normal(n) => Path::new(n),
                Component::CurDir => continue,
                // Absolute prefixes / `..` would be rejected by BENEATH anyway;
                // surface a clean error rather than attempting them.
                _ => return Err(Errno::INVAL),
            };
            match mkdirat(&dir, name, Mode::from_raw_mode(0o755)) {
                Ok(()) | Err(Errno::EXIST) => {},
                Err(e) => return Err(e),
            }
            dir = open_subdir(&dir, name)?;
        }
        Ok(())
    }

    pub(super) fn remove_file_beneath(root: &Path, rel: &Path) -> Result<(), Errno> {
        let root_fd = open_dir(root)?;
        let leaf = rel.file_name().ok_or(Errno::INVAL)?;
        let parent = rel.parent().unwrap_or_else(|| Path::new(""));
        let parent_fd = if parent.as_os_str().is_empty() {
            root_fd
        } else {
            open_subdir(&root_fd, parent)?
        };
        unlinkat(&parent_fd, Path::new(leaf), AtFlags::empty())
    }

    /// Confined atomic write: create a temp beneath the target's parent under
    /// `RESOLVE_BENEATH`, write+fsync it, then `renameat` it over the target — so
    /// the bytes hit the same inode the kernel confined (a swapped-in symlink
    /// can't redirect them) and a crash/kill mid-write leaves the previous file
    /// intact (the rename is atomic).
    pub(super) fn write_atomic_beneath(root: &Path, rel: &Path, bytes: &[u8]) -> Result<(), Errno> {
        static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

        let root_fd = open_dir(root)?;
        let leaf = rel.file_name().ok_or(Errno::INVAL)?;
        let parent = rel.parent().unwrap_or_else(|| Path::new(""));
        let parent_fd = if parent.as_os_str().is_empty() {
            root_fd
        } else {
            open_subdir(&root_fd, parent)?
        };

        let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp_name = format!(".mermaid.{}.{}.tmp", std::process::id(), n);
        let tmp_path = Path::new(&tmp_name);

        // Preserve the destination's existing permission bits. The replaced
        // `open_beneath(WriteTruncate)` kept them (O_TRUNC doesn't touch an
        // existing file's mode), so without this an `apply_patch` on a `0o755`
        // script or a `0o600` secret would silently reset it to `0o644`. A fresh
        // file keeps the `0o644` default. Statted under the confined parent fd.
        let existing_mode = statat(&parent_fd, Path::new(leaf), AtFlags::empty())
            .ok()
            .map(|st| st.st_mode & 0o7777);

        // Create the temp exclusively, beneath the confined parent.
        let fd = openat2(
            &parent_fd,
            tmp_path,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o644),
            ResolveFlags::BENEATH,
        )?;

        // Match the destination's prior mode when replacing an existing file
        // (fchmod isn't subject to umask, so the bits are exact).
        if let Some(mode) = existing_mode {
            let _ = fchmod(&fd, Mode::from_raw_mode(mode));
        }

        // Write + fsync the data; on any IO failure, unlink the temp so we don't
        // leak it, mapping the error back into an `Errno`.
        let written = (|| -> std::io::Result<()> {
            let mut file = File::from(fd);
            file.write_all(bytes)?;
            file.sync_all()
        })();
        if let Err(e) = written {
            let _ = unlinkat(&parent_fd, tmp_path, AtFlags::empty());
            return Err(io_to_errno(e));
        }

        // Atomic swap within the confined parent dir.
        if let Err(e) = renameat(&parent_fd, tmp_path, &parent_fd, Path::new(leaf)) {
            let _ = unlinkat(&parent_fd, tmp_path, AtFlags::empty());
            return Err(e);
        }

        // Best-effort durability of the rename (the directory entry itself).
        if let Ok(dir) = openat2(
            &parent_fd,
            Path::new("."),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::BENEATH,
        ) {
            let _ = File::from(dir).sync_all();
        }
        Ok(())
    }

    /// Map a std IO error to the nearest `Errno` so the confined writer can share
    /// the `NOSYS`-fallback dispatch shape of its sibling helpers.
    fn io_to_errno(e: std::io::Error) -> Errno {
        Errno::from_io_error(&e).unwrap_or(Errno::IO)
    }
}

/// Best-effort fallback used on non-Linux targets and pre-`openat2` kernels:
/// the lexical + canonical-ancestor containment check, then a plain `std::fs`
/// open by path. The TOCTOU window remains here, but the lexical guarantee does
/// not regress relative to the prior implementation.
///
/// One deliberate behavioral difference from the Linux path: because this leans
/// on `std::fs::canonicalize`, it *follows* an absolute symlink that resolves
/// back inside the root, whereas `RESOLVE_BENEATH` rejects every absolute
/// symlink outright. Both still refuse escapes — the fallback is simply more
/// permissive on that one in-tree edge case. Reconciling it would mean
/// hand-rolling per-component symlink inspection here, exactly the fragile
/// logic `openat2` exists to replace, so the mismatch is documented rather than
/// papered over.
mod fallback {
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::Once;

    use super::{OpenIntent, contain_within_canonical};

    /// Warn once per process that the kernel-confined `openat2(RESOLVE_BENEATH)`
    /// path is unavailable (non-Linux target or pre-5.6 kernel), so confinement
    /// falls back to the lexical/canonical check plus a by-path operation — which
    /// leaves the documented check-then-use symlink TOCTOU window (#142). This is
    /// by design: closing it here would mean hand-rolled per-component symlink
    /// inspection, exactly the fragile logic `openat2` exists to replace. The warn
    /// makes the residual visible to operators on those platforms.
    fn warn_fallback_once() {
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            tracing::warn!(
                "path confinement is using the by-path fallback (no openat2 \
                 RESOLVE_BENEATH on this platform/kernel); a check-then-use symlink \
                 TOCTOU window remains for in-workspace writes/deletes"
            );
        });
    }

    fn validated(root: &Path, rel: &Path) -> io::Result<PathBuf> {
        warn_fallback_once();
        let rel_str = rel
            .to_str()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "non-UTF-8 path"))?;
        contain_within_canonical(root, rel_str)
            .map_err(|e| io::Error::new(io::ErrorKind::PermissionDenied, e.to_string()))
    }

    pub(super) fn open(root: &Path, rel: &Path, intent: OpenIntent) -> io::Result<File> {
        let path = validated(root, rel)?;
        match intent {
            OpenIntent::Read => File::open(&path),
            OpenIntent::WriteTruncate => OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&path),
        }
    }

    pub(super) fn create_dir_all(root: &Path, rel: &Path) -> io::Result<()> {
        let path = validated(root, rel)?;
        std::fs::create_dir_all(&path)
    }

    pub(super) fn remove_file(root: &Path, rel: &Path) -> io::Result<()> {
        let path = validated(root, rel)?;
        std::fs::remove_file(&path)
    }

    pub(super) fn write_atomic(root: &Path, rel: &Path, bytes: &[u8]) -> io::Result<()> {
        let path = validated(root, rel)?;
        crate::write_atomic(&path, bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_parent_escape() {
        let root = std::env::temp_dir().join("mermaid_pathguard_root");
        assert!(contain_within(&root, "../escape").is_err());
    }

    #[test]
    fn rejects_absolute_outside_root() {
        let root = std::env::temp_dir().join("mermaid_pathguard_root2");
        #[cfg(unix)]
        assert!(contain_within(&root, "/etc/passwd").is_err());
        #[cfg(windows)]
        assert!(contain_within(&root, "C:\\Windows\\System32\\drivers\\etc\\hosts").is_err());
    }

    #[test]
    fn accepts_in_root_relative() {
        let root = std::env::temp_dir().join("mermaid_pathguard_root3");
        let p = contain_within(&root, "a/b.txt").unwrap();
        assert!(p.starts_with(normalize_lexical(&root)));
        assert!(p.ends_with("b.txt"));
    }

    #[test]
    fn collapses_interior_parent_within_root() {
        let root = std::env::temp_dir().join("mermaid_pathguard_root4");
        // `a/../b.txt` stays inside the root.
        let p = contain_within(&root, "a/../b.txt").unwrap();
        assert!(p.ends_with("b.txt"));
        assert!(p.starts_with(normalize_lexical(&root)));
    }
}

#[cfg(all(test, target_os = "linux"))]
mod confined_tests {
    use std::io::{Read, Write};

    use super::*;

    /// A throwaway directory unique to this test run + `tag` (tests share a PID).
    fn unique_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("mermaid_beneath_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn create_write_read_roundtrip_stays_in_root() {
        let root = unique_dir("rw");
        create_dir_all_beneath(&root, Path::new("sub/inner")).unwrap();
        {
            let mut f = open_beneath(
                &root,
                Path::new("sub/inner/file.txt"),
                OpenIntent::WriteTruncate,
            )
            .unwrap();
            f.write_all(b"hello").unwrap();
        }
        // The bytes landed at the confined inode.
        assert_eq!(
            std::fs::read_to_string(root.join("sub/inner/file.txt")).unwrap(),
            "hello"
        );
        // And read back through the confined opener.
        let mut buf = String::new();
        open_beneath(&root, Path::new("sub/inner/file.txt"), OpenIntent::Read)
            .unwrap()
            .read_to_string(&mut buf)
            .unwrap();
        assert_eq!(buf, "hello");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn open_beneath_refuses_write_through_escaping_symlink() {
        let root = unique_dir("escape_root");
        let outside = unique_dir("escape_outside");
        // Plant a symlink *inside* the root that redirects outside it — the
        // exact TOCTOU shape #77 is about.
        std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();

        let res = open_beneath(
            &root,
            Path::new("escape/evil.txt"),
            OpenIntent::WriteTruncate,
        );
        assert!(
            res.is_err(),
            "write through escaping symlink must be refused"
        );
        assert!(
            !outside.join("evil.txt").exists(),
            "nothing should have been written outside the root"
        );

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn open_beneath_follows_in_tree_symlink() {
        // The crux of choosing RESOLVE_BENEATH over RESOLVE_NO_SYMLINKS: a
        // symlink that stays *inside* the root (the node_modules / monorepo
        // case) must still resolve. A relative in-tree link is followed; only
        // escapes are refused.
        let root = unique_dir("intree");
        std::fs::create_dir(root.join("real")).unwrap();
        // Relative target so resolution stays beneath the root.
        std::os::unix::fs::symlink("real", root.join("link")).unwrap();

        {
            let mut f =
                open_beneath(&root, Path::new("link/file.txt"), OpenIntent::WriteTruncate).unwrap();
            f.write_all(b"via-symlink").unwrap();
        }
        // The bytes landed in the real directory the in-tree link points at.
        assert_eq!(
            std::fs::read_to_string(root.join("real/file.txt")).unwrap(),
            "via-symlink"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn create_dir_all_beneath_refuses_escape() {
        let root = unique_dir("mkdir_root");
        let outside = unique_dir("mkdir_outside");
        std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();

        let res = create_dir_all_beneath(&root, Path::new("escape/newdir"));
        assert!(
            res.is_err(),
            "mkdir through escaping symlink must be refused"
        );
        assert!(!outside.join("newdir").exists());

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn remove_file_beneath_deletes_in_root_and_refuses_escape() {
        let root = unique_dir("rm_root");
        {
            let mut f =
                open_beneath(&root, Path::new("gone.txt"), OpenIntent::WriteTruncate).unwrap();
            f.write_all(b"x").unwrap();
        }
        assert!(root.join("gone.txt").exists());
        remove_file_beneath(&root, Path::new("gone.txt")).unwrap();
        assert!(!root.join("gone.txt").exists());

        // A victim outside the root, reachable only via an escaping symlink, is
        // safe from a confined unlink.
        let outside = unique_dir("rm_outside");
        std::fs::write(outside.join("victim.txt"), b"keep").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();
        let res = remove_file_beneath(&root, Path::new("escape/victim.txt"));
        assert!(
            res.is_err(),
            "unlink through escaping symlink must be refused"
        );
        assert!(outside.join("victim.txt").exists());

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn write_atomic_beneath_roundtrips_and_replaces_without_temp_residue() {
        let root = unique_dir("atomic_rw");
        create_dir_all_beneath(&root, Path::new("sub")).unwrap();
        write_atomic_beneath(&root, Path::new("sub/file.txt"), b"first").unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("sub/file.txt")).unwrap(),
            "first"
        );
        // Overwriting is an atomic swap, not a truncate-in-place.
        write_atomic_beneath(&root, Path::new("sub/file.txt"), b"second").unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("sub/file.txt")).unwrap(),
            "second"
        );
        // No `.tmp` sibling left behind.
        let leftovers = std::fs::read_dir(root.join("sub"))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0, "atomic write must not leak a temp file");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn write_atomic_beneath_preserves_existing_mode() {
        use std::os::unix::fs::PermissionsExt;
        let root = unique_dir("atomic_mode");
        let rel = Path::new("script.sh");
        write_atomic_beneath(&root, rel, b"#!/bin/sh\n").unwrap();
        // Make it executable, then rewrite it: the atomic swap must keep 0o755
        // rather than resetting to the temp's 0o644 (an apply_patch on a script
        // must not strip the executable bit).
        std::fs::set_permissions(
            root.join("script.sh"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        write_atomic_beneath(&root, rel, b"#!/bin/sh\necho hi\n").unwrap();
        let mode = std::fs::metadata(root.join("script.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o755, "atomic write must preserve the executable bit");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn write_atomic_beneath_refuses_write_through_escaping_symlink() {
        let root = unique_dir("atomic_escape_root");
        let outside = unique_dir("atomic_escape_outside");
        std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();

        let res = write_atomic_beneath(&root, Path::new("escape/evil.txt"), b"x");
        assert!(
            res.is_err(),
            "atomic write through escaping symlink must be refused"
        );
        assert!(!outside.join("evil.txt").exists());

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn write_atomic_beneath_follows_in_tree_symlink() {
        let root = unique_dir("atomic_intree");
        std::fs::create_dir(root.join("real")).unwrap();
        std::os::unix::fs::symlink("real", root.join("link")).unwrap();

        write_atomic_beneath(&root, Path::new("link/file.txt"), b"via-symlink").unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("real/file.txt")).unwrap(),
            "via-symlink"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
