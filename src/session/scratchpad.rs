//! Per-session scratch directories.
//!
//! Each chat session gets a private on-disk scratch area keyed by its
//! conversation id: `<system-temp>/mermaid-<uid>/<project-slug>/<session-id>/scratchpad`.
//! Tools and spawned subprocesses use it for intermediate files instead
//! of the shared system temp dir. Living under the system temp dir means
//! tmpfs speed where the OS provides it and a free wipe on reboot; the
//! `mermaid-<uid>` root and each session dir are tightened to `0700` so
//! other local users can neither read nor pre-create paths inside them.
//!
//! Lifecycle: the reducer emits `Cmd::EnsureScratchpad` at startup and
//! whenever the conversation id changes (`/clear`, `/load`, rewind fork);
//! the effect layer materializes the directory here and reports it back
//! via `Msg::ScratchpadReady`, which stamps `Session::scratchpad`. A
//! `.lock` file in the session dir — deliberately *outside* the advertised
//! `scratchpad/` child, so a stray `rm -rf $MERMAID_SCRATCHPAD` cannot
//! remove it — is held via `File::try_lock` for the process lifetime and
//! marks the directory as in use; [`sweep_stale`] reaps unlocked
//! directories older than the retention window so abandoned sessions
//! don't accumulate forever.

use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// Lock marking a session directory as owned by a live process. Sits next
/// to (not inside) the advertised `scratchpad/` child.
const LOCK_FILE: &str = ".lock";
/// The advertised directory itself, as a child of the locked session dir.
const SCRATCH_SUBDIR: &str = "scratchpad";
/// Default retention: unlocked scratchpads older than this are reaped by
/// [`sweep_stale`]. mermaidd overrides it via `daemon.scratchpad_retention_days`.
pub const RETENTION_DAYS: u64 = 7;
/// Cap on a sanitized path component — keeps the full scratchpad path
/// well under PATH_MAX even for deeply nested project directories.
const MAX_COMPONENT_LEN: usize = 96;
/// Cap on the `/scratchpad` listing — keeps a scratch dir full of build
/// output from flooding the transcript.
const MAX_LIST_ENTRIES: usize = 100;

/// Locks this process holds, keyed by session dir. Holding the open
/// `File` keeps the OS lock alive for the process lifetime (and releases
/// it automatically on crash); the map makes `ensure` idempotent — a
/// second `try_lock` on a path we already own would spuriously read as
/// "held by someone else".
static HELD_LOCKS: OnceLock<Mutex<HashMap<PathBuf, File>>> = OnceLock::new();

/// Per-user scratchpad root under the system temp dir. The uid suffix
/// keeps roots disjoint on shared unix hosts; on Windows `%TEMP%` is
/// already per-user, so a plain `mermaid` suffices.
fn scratch_root_in(temp: &Path) -> PathBuf {
    #[cfg(unix)]
    {
        temp.join(format!("mermaid-{}", rustix::process::getuid().as_raw()))
    }
    #[cfg(not(unix))]
    {
        temp.join("mermaid")
    }
}

fn scratch_root() -> PathBuf {
    scratch_root_in(&std::env::temp_dir())
}

/// Flatten a project path into a single filesystem-safe component
/// (`/home/user/my proj` -> `-home-user-my-proj`). Never empty: a
/// degenerate input falls back to `"project"`.
pub fn project_slug(project: &Path) -> String {
    sanitize_component(&project.display().to_string(), "project")
}

/// One path component: alphanumerics, `-` and `_` pass through, every
/// other byte becomes `-`. Sanitizing (rather than trusting) the input
/// means a hostile string like `../../x` can never traverse out of the
/// scratchpad root. Truncation is char-boundary-safe by construction
/// (the output is pure ASCII).
fn sanitize_component(raw: &str, fallback: &str) -> String {
    let mut out: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    out.truncate(MAX_COMPONENT_LEN);
    if out.is_empty() {
        fallback.to_string()
    } else {
        out
    }
}

/// Pure path computation: the locked session dir for
/// `(project, session_id)` under `root`. No filesystem access. The
/// advertised scratchpad is its [`SCRATCH_SUBDIR`] child.
pub fn session_dir(root: &Path, project: &Path, session_id: &str) -> PathBuf {
    root.join(project_slug(project))
        .join(sanitize_component(session_id, "session"))
}

/// Create (or adopt) the scratchpad for this project + session id, take
/// its liveness lock, and return the advertised `scratchpad/` path.
/// Idempotent — re-running for the same session keeps the already-held
/// lock and returns the same path. If another live process holds the
/// lock (the same conversation open twice), the directory is shared and
/// that process's lock protects it.
pub fn ensure(project: &Path, session_id: &str) -> io::Result<PathBuf> {
    ensure_in(&scratch_root(), project, session_id)
}

/// [`ensure`] against an explicit root, so tests can point it at a
/// throwaway directory (same pattern as mermaidd's bg-log sweep).
fn ensure_in(root: &Path, project: &Path, session_id: &str) -> io::Result<PathBuf> {
    let dir = session_dir(root, project, session_id);
    let scratch = dir.join(SCRATCH_SUBDIR);
    std::fs::create_dir_all(&scratch)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Best-effort tighten each level to owner-only — cheap, and
        // self-heals bits loosened by an aggressive umask (mirrors
        // `private_temp_dir`). Chain: root -> slug -> session -> scratchpad.
        for level in [root, dir.parent().unwrap_or(root), &dir, &scratch] {
            let _ = std::fs::set_permissions(level, std::fs::Permissions::from_mode(0o700));
        }
    }
    let held = HELD_LOCKS.get_or_init(Mutex::default);
    let mut held = held.lock().expect("scratchpad lock registry poisoned");
    if !held.contains_key(&dir) {
        let lock = File::create(dir.join(LOCK_FILE))?;
        match lock.try_lock() {
            // Hold for the process lifetime; dropped (and OS-released) only
            // at exit or crash.
            Ok(()) => {
                held.insert(dir.clone(), lock);
            },
            // Another live mermaid owns this session dir; its lock protects
            // the directory, so sharing it unlocked is fine.
            Err(std::fs::TryLockError::WouldBlock) => {},
            Err(std::fs::TryLockError::Error(err)) => return Err(err),
        }
    }
    Ok(scratch)
}

/// Is the session dir's lock held by a live process (this one included)?
/// Missing or unlockable-for-io lock files read as "not live" — the age
/// check in the sweep still bounds how quickly such a dir can be reaped.
fn lock_is_held(dir: &Path) -> bool {
    if let Ok(held) = HELD_LOCKS.get_or_init(Mutex::default).lock()
        && held.contains_key(dir)
    {
        return true;
    }
    let Ok(lock) = File::open(dir.join(LOCK_FILE)) else {
        return false;
    };
    match lock.try_lock() {
        Err(std::fs::TryLockError::WouldBlock) => true,
        // Acquired (or unreadable): no live owner. The lock drops here,
        // releasing immediately.
        _ => false,
    }
}

/// Reap unheld scratchpads older than `retention_days`. Returns the
/// number of session directories removed. Runs on session startup (the
/// `EnsureScratchpad` effect, with [`RETENTION_DAYS`]) and on mermaidd
/// startup (with the daemon's configured retention) — no separate timer.
pub fn sweep_stale(retention_days: u64) -> io::Result<u64> {
    sweep_stale_in(&scratch_root(), retention_days)
}

/// [`sweep_stale`] against an explicit root. Public so mermaidd's tests (a
/// separate bin target that can't see `pub(crate)`) can drive the sweep
/// against a fixture directory, mirroring its bg-log sweep tests.
pub fn sweep_stale_in(root: &Path, retention_days: u64) -> io::Result<u64> {
    if !root.exists() {
        return Ok(0);
    }
    let cutoff = Duration::from_secs(retention_days * 24 * 60 * 60);
    let mut removed = 0u64;
    for project in std::fs::read_dir(root)? {
        let project = project?;
        if !project.file_type()?.is_dir() {
            continue;
        }
        for session in std::fs::read_dir(project.path())? {
            let session = session?;
            let dir = session.path();
            if !session.file_type()?.is_dir() {
                continue;
            }
            // A live owner protects the directory regardless of age (a
            // week-long session keeps its scratchpad).
            if lock_is_held(&dir) {
                continue;
            }
            // Age from the directory's mtime; a clock skewed into the
            // future reads as "fresh" (elapsed errors) — keep, fail open.
            let stale = session
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|mtime| mtime.elapsed().ok())
                .is_some_and(|age| age >= cutoff);
            if stale && std::fs::remove_dir_all(&dir).is_ok() {
                removed += 1;
            }
        }
        // Best-effort: drop a project dir the sweep just emptied (fails
        // harmlessly while any session dir remains).
        let _ = std::fs::remove_dir(project.path());
    }
    Ok(removed)
}

/// Remove one session's scratchpad — the delete-conversation cascade. A
/// directory whose lock is held by a live process is left alone (that
/// session is still open, possibly in another mermaid); the sweep reaps
/// it later.
pub fn remove(project: &Path, session_id: &str) -> io::Result<()> {
    remove_in(&scratch_root(), project, session_id)
}

/// [`remove`] against an explicit root, for tests.
fn remove_in(root: &Path, project: &Path, session_id: &str) -> io::Result<()> {
    let dir = session_dir(root, project, session_id);
    if !dir.exists() || lock_is_held(&dir) {
        return Ok(());
    }
    std::fs::remove_dir_all(&dir)
}

/// Bounded ASCII listing of a scratchpad's contents, for `/scratchpad`.
/// Deterministic (sorted, directories recursed depth-first), relative
/// paths, human-readable sizes, capped at [`MAX_LIST_ENTRIES`] lines with
/// an explicit "more" marker. The lock file lives outside the advertised
/// directory, so everything found here is user content.
pub fn list_text(dir: &Path) -> String {
    let mut out = format!("Scratchpad: {}", dir.display());
    let mut entries = Vec::new();
    let mut truncated = false;
    collect_entries(dir, "", &mut entries, &mut truncated);
    if entries.is_empty() {
        out.push_str("\n  (empty)");
        return out;
    }
    for line in &entries {
        out.push_str("\n  ");
        out.push_str(line);
    }
    if truncated {
        out.push_str("\n  ... (listing capped)");
    }
    out
}

/// Depth-first sorted walk feeding [`list_text`]; stops (setting
/// `truncated`) once the entry cap is hit. Unreadable directories are
/// skipped rather than failing the whole listing. `rel` is a `/`-joined
/// string (not a `PathBuf`) so the listing renders identically on every
/// OS — `Path::display` would print `a\nested.log` on Windows.
fn collect_entries(dir: &Path, rel: &str, entries: &mut Vec<String>, truncated: &mut bool) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    let mut children: Vec<_> = read.flatten().map(|e| e.path()).collect();
    children.sort();
    for path in children {
        if entries.len() >= MAX_LIST_ENTRIES {
            *truncated = true;
            return;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let child_rel = if rel.is_empty() {
            name.to_string()
        } else {
            format!("{rel}/{name}")
        };
        if path.is_dir() {
            entries.push(format!("{child_rel}/"));
            collect_entries(&path, &child_rel, entries, truncated);
        } else {
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            entries.push(format!("{child_rel} ({})", human_size(size)));
        }
    }
}

/// `1023 B` / `1.5 KB` / `2.0 MB` — plain ASCII, one decimal past bytes.
fn human_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Throwaway root per test, following the mermaidd sweep-test pattern.
    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mermaid_scratchpad_{}_{}",
            name,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// Drop this process's held lock for a session dir, simulating the
    /// owning process having exited (the OS releases the flock with it).
    fn release_lock(session_dir: &Path) {
        HELD_LOCKS
            .get_or_init(Mutex::default)
            .lock()
            .expect("registry")
            .remove(session_dir);
    }

    #[test]
    fn scratch_root_is_per_user_under_system_temp() {
        let root = scratch_root_in(Path::new("/tmp"));
        #[cfg(unix)]
        assert_eq!(
            root,
            Path::new("/tmp").join(format!("mermaid-{}", rustix::process::getuid().as_raw()))
        );
        #[cfg(not(unix))]
        assert_eq!(root, Path::new("/tmp").join("mermaid"));
    }

    #[test]
    fn project_slug_table() {
        let cases: &[(&str, &str)] = &[
            ("/home/user/my proj", "-home-user-my-proj"),
            ("/", "-"),
            ("relative-dir", "relative-dir"),
            ("under_score", "under_score"),
            ("", "project"),
        ];
        for (input, expected) in cases {
            assert_eq!(
                project_slug(Path::new(input)),
                *expected,
                "slug of {input:?}"
            );
        }
    }

    #[test]
    fn project_slug_truncates_long_paths() {
        let long = "/a".repeat(200);
        let slug = project_slug(Path::new(&long));
        assert_eq!(slug.len(), MAX_COMPONENT_LEN);
    }

    #[test]
    fn session_dir_confines_a_hostile_session_id() {
        // Conversation ids come from on-disk filenames on --resume; a
        // crafted `../../evil` must sanitize into a plain component that
        // stays beneath the root instead of traversing out of it.
        let root = Path::new("/data/scratchpad");
        let dir = session_dir(root, Path::new("/proj"), "../../evil");
        assert!(dir.starts_with(root.join("-proj")));
        assert!(
            dir.components()
                .all(|c| !matches!(c, std::path::Component::ParentDir))
        );
    }

    #[test]
    fn ensure_creates_the_advertised_child_and_holds_the_lock() {
        let root = temp_root("ensure");
        let scratch = ensure_in(&root, Path::new("/proj"), "20260710_120000_000").expect("ensure");
        assert!(scratch.is_dir());
        assert!(
            scratch.ends_with(SCRATCH_SUBDIR),
            "advertised path is the scratchpad child: {}",
            scratch.display()
        );
        let session = scratch.parent().expect("session dir");
        assert!(
            session.join(LOCK_FILE).is_file(),
            "lock sits OUTSIDE the advertised dir"
        );
        assert!(lock_is_held(session), "ensure holds the liveness lock");
        // Idempotent: same inputs, same path, lock still held once.
        let again = ensure_in(&root, Path::new("/proj"), "20260710_120000_000").expect("ensure");
        assert_eq!(scratch, again);
        release_lock(session);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn ensure_tightens_perms_to_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_root("perms");
        let scratch = ensure_in(&root, Path::new("/proj"), "20260710_120000_000").expect("ensure");
        let session = scratch.parent().expect("session dir").to_path_buf();
        for level in [&root, &session, &scratch] {
            let mode = std::fs::metadata(level).expect("meta").permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "mode of {}", level.display());
        }
        release_lock(&session);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sweep_lock_table() {
        // Retention 0 makes every directory "old enough", so the lock is
        // the only thing standing between a dir and removal — which is
        // exactly the property under test. The "fresh" row uses a huge
        // retention instead of forging mtimes (portable across CI OSes).
        let root = temp_root("sweep");
        let live = ensure_in(&root, Path::new("/proj"), "live").expect("ensure");
        let live_session = live.parent().expect("session").to_path_buf();
        let abandoned = ensure_in(&root, Path::new("/proj"), "abandoned").expect("ensure");
        let abandoned_session = abandoned.parent().expect("session").to_path_buf();
        release_lock(&abandoned_session); // owner "exited"
        let lockless = ensure_in(&root, Path::new("/proj"), "lockless").expect("ensure");
        let lockless_session = lockless.parent().expect("session").to_path_buf();
        release_lock(&lockless_session);
        std::fs::remove_file(lockless_session.join(LOCK_FILE)).expect("drop lock file");

        let removed = sweep_stale_in(&root, 0).expect("sweep");
        assert_eq!(removed, 2, "released + lockless reaped");
        assert!(live.is_dir(), "a held lock protects the dir");
        assert!(!abandoned_session.exists());
        assert!(!lockless_session.exists());

        // Fresh + unheld survives a normal retention window.
        let fresh = ensure_in(&root, Path::new("/proj"), "fresh").expect("ensure");
        let fresh_session = fresh.parent().expect("session").to_path_buf();
        release_lock(&fresh_session);
        let removed = sweep_stale_in(&root, RETENTION_DAYS).expect("sweep");
        assert_eq!(removed, 0);
        assert!(fresh.is_dir(), "young dirs are kept even without a lock");
        release_lock(&live_session);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sweep_of_a_missing_root_is_a_noop() {
        let root = temp_root("missing");
        assert_eq!(sweep_stale_in(&root, 0).expect("sweep"), 0);
    }

    #[test]
    fn remove_cascades_unheld_dirs_but_spares_live_ones() {
        let root = temp_root("remove");
        // Held lock = "session open somewhere" — spared.
        let live = ensure_in(&root, Path::new("/proj"), "live").expect("ensure");
        let live_session = live.parent().expect("session").to_path_buf();
        remove_in(&root, Path::new("/proj"), "live").expect("remove");
        assert!(live.is_dir(), "a held lock protects the dir from cascade");
        // Released = abandoned — removed regardless of age.
        let gone = ensure_in(&root, Path::new("/proj"), "gone").expect("ensure");
        let gone_session = gone.parent().expect("session").to_path_buf();
        release_lock(&gone_session);
        remove_in(&root, Path::new("/proj"), "gone").expect("remove");
        assert!(!gone_session.exists());
        // Missing dir is a noop, not an error.
        remove_in(&root, Path::new("/proj"), "never-existed").expect("remove");
        release_lock(&live_session);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn list_text_is_sorted_and_bounded() {
        let root = temp_root("list");
        let dir = ensure_in(&root, Path::new("/proj"), "s").expect("ensure");
        let session = dir.parent().expect("session").to_path_buf();
        assert_eq!(
            list_text(&dir),
            format!("Scratchpad: {}\n  (empty)", dir.display()),
            "a fresh scratchpad reads as empty (the lock is outside it)"
        );
        std::fs::write(dir.join("b.txt"), b"hello").expect("write");
        std::fs::create_dir(dir.join("a")).expect("mkdir");
        std::fs::write(dir.join("a").join("nested.log"), vec![0u8; 2048]).expect("write");
        let text = list_text(&dir);
        assert!(text.is_ascii(), "listing must be pure ASCII");
        assert_eq!(
            text,
            format!(
                "Scratchpad: {}\n  a/\n  a/nested.log (2.0 KB)\n  b.txt (5 B)",
                dir.display()
            )
        );
        // Cap: many files -> exactly MAX_LIST_ENTRIES lines plus a marker.
        for i in 0..(MAX_LIST_ENTRIES + 10) {
            std::fs::write(dir.join(format!("f{i:04}.tmp")), b"x").expect("write");
        }
        let text = list_text(&dir);
        assert_eq!(
            text.lines().count(),
            1 + MAX_LIST_ENTRIES + 1,
            "header + cap + marker"
        );
        assert!(text.ends_with("... (listing capped)"));
        release_lock(&session);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn human_size_units() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(3 * 1024 * 1024), "3.0 MB");
    }
}
