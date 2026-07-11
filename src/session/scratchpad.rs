//! Per-session scratch directories.
//!
//! Each chat session gets a private on-disk scratch area keyed by its
//! conversation id: `<data>/tmp/scratchpad/<project-slug>/<session-id>`.
//! Tools and spawned subprocesses use it for intermediate files instead
//! of the shared system temp dir. The whole tree lives under the `0700`
//! [`private_temp_dir`](crate::utils::private_temp_dir), so other local
//! users can neither read nor pre-create paths inside it.
//!
//! Lifecycle: the reducer emits `Cmd::EnsureScratchpad` at startup and
//! whenever the conversation id changes (`/clear`, `/load`, rewind fork);
//! the effect layer materializes the directory here and reports it back
//! via `Msg::ScratchpadReady`, which stamps `Session::scratchpad`. A
//! `.lock` file holding the owning pid marks a directory as in use;
//! [`sweep_stale`] reaps unlocked directories older than the retention
//! window so abandoned sessions don't accumulate forever.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Directory under `private_temp_dir()` holding every scratchpad.
const ROOT_DIR: &str = "scratchpad";
/// Pid lock marking a session directory as owned by a live process.
const LOCK_FILE: &str = ".lock";
/// Default retention: unlocked scratchpads older than this are reaped by
/// [`sweep_stale`]. mermaidd overrides it via `daemon.scratchpad_retention_days`.
pub const RETENTION_DAYS: u64 = 7;
/// Cap on a sanitized path component — keeps the full scratchpad path
/// well under PATH_MAX even for deeply nested project directories.
const MAX_COMPONENT_LEN: usize = 96;
/// Cap on the `/scratchpad` listing — keeps a scratch dir full of build
/// output from flooding the transcript.
const MAX_LIST_ENTRIES: usize = 100;

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

/// Pure path computation: where the scratchpad for `(project, session_id)`
/// lives under `root`. No filesystem access.
pub fn session_dir(root: &Path, project: &Path, session_id: &str) -> PathBuf {
    root.join(project_slug(project))
        .join(sanitize_component(session_id, "session"))
}

/// Create (or adopt) the scratchpad for this project + session id and
/// stamp its pid lock. Idempotent — re-running for the same session
/// refreshes the lock and returns the same path.
pub fn ensure(project: &Path, session_id: &str) -> io::Result<PathBuf> {
    let root = crate::utils::private_temp_dir()?.join(ROOT_DIR);
    ensure_in(&root, project, session_id)
}

/// [`ensure`] against an explicit root, so tests can point it at a
/// throwaway directory (same pattern as mermaidd's bg-log sweep).
fn ensure_in(root: &Path, project: &Path, session_id: &str) -> io::Result<PathBuf> {
    let dir = session_dir(root, project, session_id);
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Best-effort tighten each level to owner-only — cheap, and
        // self-heals bits loosened by an aggressive umask (mirrors
        // `private_temp_dir`). The parent chain is root -> slug -> session.
        for level in [root, dir.parent().unwrap_or(root), dir.as_ref()] {
            let _ = std::fs::set_permissions(level, std::fs::Permissions::from_mode(0o700));
        }
    }
    // The lock carries the owning pid so the sweep can tell "in use by a
    // live mermaid process" from "abandoned by a crashed one". Plain
    // overwrite: the newest owner of a session id wins.
    std::fs::write(dir.join(LOCK_FILE), std::process::id().to_string())?;
    Ok(dir)
}

/// Reap unlocked scratchpads older than `retention_days`. Returns the
/// number of session directories removed. Runs on session startup (the
/// `EnsureScratchpad` effect, with [`RETENTION_DAYS`]) and on mermaidd
/// startup (with the daemon's configured retention) — no separate timer.
pub fn sweep_stale(retention_days: u64) -> io::Result<u64> {
    let root = crate::utils::private_temp_dir()?.join(ROOT_DIR);
    sweep_stale_in(&root, retention_days)
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
            if lock_pid(&dir).is_some_and(pid_alive) {
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
/// directory whose lock names a live process is left alone (that session is
/// still open, possibly in another mermaid); the sweep reaps it later.
pub fn remove(project: &Path, session_id: &str) -> io::Result<()> {
    let root = crate::utils::private_temp_dir()?.join(ROOT_DIR);
    remove_in(&root, project, session_id)
}

/// [`remove`] against an explicit root, for tests.
fn remove_in(root: &Path, project: &Path, session_id: &str) -> io::Result<()> {
    let dir = session_dir(root, project, session_id);
    if !dir.exists() || lock_pid(&dir).is_some_and(pid_alive) {
        return Ok(());
    }
    std::fs::remove_dir_all(&dir)
}

/// Bounded ASCII listing of a scratchpad's contents, for `/scratchpad`.
/// Deterministic (sorted, directories recursed depth-first), relative
/// paths, human-readable sizes, capped at [`MAX_LIST_ENTRIES`] lines with
/// an explicit "more" marker. The internal `.lock` file is elided.
pub fn list_text(dir: &Path) -> String {
    let mut out = format!("Scratchpad: {}", dir.display());
    let mut entries = Vec::new();
    let mut truncated = false;
    collect_entries(dir, Path::new(""), &mut entries, &mut truncated);
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
/// skipped rather than failing the whole listing.
fn collect_entries(dir: &Path, rel: &Path, entries: &mut Vec<String>, truncated: &mut bool) {
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
        if rel.as_os_str().is_empty() && name == LOCK_FILE {
            continue; // internal pid lock, not user content
        }
        let child_rel = rel.join(name);
        if path.is_dir() {
            entries.push(format!("{}/", child_rel.display()));
            collect_entries(&path, &child_rel, entries, truncated);
        } else {
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            entries.push(format!("{} ({})", child_rel.display(), human_size(size)));
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

/// The pid recorded in a session directory's lock file, if parseable.
fn lock_pid(dir: &Path) -> Option<u32> {
    let raw = std::fs::read_to_string(dir.join(LOCK_FILE)).ok()?;
    raw.trim().parse::<u32>().ok()
}

/// Is the lock's owner still running? Everything under the 0700 root
/// belongs to this user, so on Unix a plain `kill(pid, 0)` probe suffices
/// (EPERM can't mean "someone else's live process" here). On non-Unix
/// only our own pid is provably alive; other dirs fall back to the age
/// check, which the retention window already bounds.
fn pid_alive(pid: u32) -> bool {
    if pid == std::process::id() {
        return true;
    }
    #[cfg(unix)]
    {
        // A lock value outside the valid pid range (0, or > i32::MAX —
        // `Pid::from_raw` debug-asserts on negatives) can't be a live
        // process; read it as "not alive" and let the age check decide.
        let Ok(raw) = i32::try_from(pid) else {
            return false;
        };
        match rustix::process::Pid::from_raw(raw) {
            Some(p) => rustix::process::test_kill_process(p).is_ok(),
            None => false,
        }
    }
    #[cfg(not(unix))]
    {
        false
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
    fn ensure_creates_the_dir_and_stamps_the_pid_lock() {
        let root = temp_root("ensure");
        let dir = ensure_in(&root, Path::new("/proj"), "20260710_120000_000").expect("ensure");
        assert!(dir.is_dir());
        assert_eq!(
            lock_pid(&dir),
            Some(std::process::id()),
            "lock file carries the owning pid"
        );
        // Idempotent: same inputs, same path.
        let again = ensure_in(&root, Path::new("/proj"), "20260710_120000_000").expect("ensure");
        assert_eq!(dir, again);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn ensure_tightens_perms_to_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_root("perms");
        let dir = ensure_in(&root, Path::new("/proj"), "20260710_120000_000").expect("ensure");
        for level in [&root, &dir] {
            let mode = std::fs::metadata(level).expect("meta").permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "mode of {}", level.display());
        }
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
        let unlocked = ensure_in(&root, Path::new("/proj"), "unlocked").expect("ensure");
        std::fs::remove_file(unlocked.join(LOCK_FILE)).expect("drop lock");
        let dead = ensure_in(&root, Path::new("/proj"), "dead").expect("ensure");
        // Far above any real pid_max (Linux caps at 2^22) — reliably not
        // running, while still a valid i32 so the real kill probe runs.
        std::fs::write(dead.join(LOCK_FILE), "999999999").expect("forge pid");
        let garbled = ensure_in(&root, Path::new("/proj"), "garbled").expect("ensure");
        std::fs::write(garbled.join(LOCK_FILE), "not a pid").expect("garble");

        let removed = sweep_stale_in(&root, 0).expect("sweep");
        assert_eq!(removed, 3, "unlocked + dead-pid + garbled-lock reaped");
        assert!(live.is_dir(), "live-pid lock protects the dir");
        assert!(!unlocked.exists());
        assert!(!dead.exists());
        assert!(!garbled.exists());

        // Fresh + unlocked survives a normal retention window.
        let fresh = ensure_in(&root, Path::new("/proj"), "fresh").expect("ensure");
        std::fs::remove_file(fresh.join(LOCK_FILE)).expect("drop lock");
        let removed = sweep_stale_in(&root, RETENTION_DAYS).expect("sweep");
        assert_eq!(removed, 0);
        assert!(fresh.is_dir(), "young dirs are kept even without a lock");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sweep_of_a_missing_root_is_a_noop() {
        let root = temp_root("missing");
        assert_eq!(sweep_stale_in(&root, 0).expect("sweep"), 0);
    }

    #[test]
    fn remove_cascades_unlocked_dirs_but_spares_live_ones() {
        let root = temp_root("remove");
        // Own-pid lock = "session open somewhere" — spared.
        let live = ensure_in(&root, Path::new("/proj"), "live").expect("ensure");
        remove_in(&root, Path::new("/proj"), "live").expect("remove");
        assert!(live.is_dir(), "a live lock protects the dir from cascade");
        // Unlocked = abandoned — removed regardless of age.
        let gone = ensure_in(&root, Path::new("/proj"), "gone").expect("ensure");
        std::fs::remove_file(gone.join(LOCK_FILE)).expect("drop lock");
        remove_in(&root, Path::new("/proj"), "gone").expect("remove");
        assert!(!gone.exists());
        // Missing dir is a noop, not an error.
        remove_in(&root, Path::new("/proj"), "never-existed").expect("remove");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn list_text_is_sorted_bounded_and_elides_the_lock() {
        let root = temp_root("list");
        let dir = ensure_in(&root, Path::new("/proj"), "s").expect("ensure");
        assert_eq!(
            list_text(&dir),
            format!("Scratchpad: {}\n  (empty)", dir.display()),
            "the pid lock alone reads as empty"
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
