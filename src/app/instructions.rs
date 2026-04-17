//! MERMAID.md project-instructions loader (Step 5h).
//!
//! On session start, walk UP from the current working directory looking
//! for `MERMAID.md`. Stop at the git root (any directory containing a
//! `.git` entry) or at `$HOME`, whichever is reached first. Load the
//! nearest file (single file wins — no merging up the tree); cap at
//! `MAX_INSTRUCTIONS_BYTES`; pass the content to the model as a dynamic
//! suffix on the system prompt.
//!
//! Auto-reload: before every model call, `refresh()` stats the loaded
//! file's path and compares mtime. If the mtime moved, re-read; if the
//! file is gone, drop the instructions. One stat per turn is
//! microseconds — no need for a filesystem watcher.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::constants::{INSTRUCTIONS_TRUNCATION_MARKER, MAX_INSTRUCTIONS_BYTES};

/// Filename Mermaid looks for. Single canonical name in Step 5h —
/// alternatives like `AGENTS.md` / `CLAUDE.md` deferred.
const INSTRUCTIONS_FILENAME: &str = "MERMAID.md";

/// Hard cap on how many directory levels `find_mermaid_md` walks up
/// before giving up. Guards against pathological symlink loops.
const MAX_WALK_DEPTH: usize = 32;

/// One-shot snapshot of the loaded MERMAID.md. Stored on `App` and
/// `NonInteractiveRunner` so the per-turn auto-reload check has
/// something to compare against.
#[derive(Debug, Clone)]
pub struct LoadedInstructions {
    /// Absolute path the content was read from.
    pub path: PathBuf,
    /// File body, possibly truncated. The truncation marker is
    /// appended in-place so the model sees the elision.
    pub content: String,
    /// mtime at last read — compared against the next `stat()` to
    /// decide whether to re-read.
    pub mtime: SystemTime,
    /// Original file size on disk (before any truncation).
    pub byte_len: usize,
    /// True when the file was larger than `MAX_INSTRUCTIONS_BYTES`
    /// and the content was clipped + marker appended.
    pub truncated: bool,
}

impl LoadedInstructions {
    /// Approximate token count for status messages. ~4 chars/token is
    /// the rule of thumb that's correct enough for user-facing display.
    pub fn approx_tokens(&self) -> usize {
        self.content.len() / 4
    }
}

/// Outcome of a `refresh()` call. Used to decide whether to emit a
/// status line so the user knows their context shifted.
#[derive(Debug, PartialEq, Eq)]
pub enum ReloadOutcome {
    /// File still has the same mtime (or was/still is absent).
    Unchanged,
    /// File was loaded for the first time this session — handles "user
    /// created MERMAID.md mid-session" gracefully.
    LoadedFirst { tokens: usize },
    /// File content changed since the last read.
    Reloaded {
        old_tokens: usize,
        new_tokens: usize,
    },
    /// File was previously loaded but has been deleted from disk.
    Removed,
}

/// Walk UP from `start` looking for `MERMAID.md`. Stops at the first of:
/// - a directory containing `.git` (the git root)
/// - `$HOME` (don't search above the user's home)
/// - filesystem root
/// - `MAX_WALK_DEPTH` levels (symlink-loop guard)
///
/// Returns the absolute path of the first MERMAID.md found, or `None`
/// if no MERMAID.md exists in the bounded walk.
pub fn find_mermaid_md(start: &Path) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut current = start.to_path_buf();
    for _ in 0..MAX_WALK_DEPTH {
        // Check this directory for MERMAID.md.
        let candidate = current.join(INSTRUCTIONS_FILENAME);
        if candidate.is_file() {
            return Some(candidate);
        }
        // Stop at the git root (the .git entry itself ends the walk;
        // most projects vendor a single MERMAID.md at the repo root).
        if current.join(".git").exists() {
            return None;
        }
        // Stop at $HOME — don't search the user's home directory or
        // anything above it. Avoids accidentally picking up a
        // long-forgotten MERMAID.md from a sibling project.
        if let Some(ref h) = home
            && current == *h
        {
            return None;
        }
        // Move up one level. If we're at the filesystem root, stop.
        match current.parent() {
            Some(parent) if parent != current => current = parent.to_path_buf(),
            _ => return None,
        }
    }
    None
}

/// Read the file at `path`, truncate to `MAX_INSTRUCTIONS_BYTES` if
/// oversized, and return a `LoadedInstructions`. Returns `None` if the
/// file can't be read or doesn't exist.
pub fn load_from_path(path: &Path) -> Option<LoadedInstructions> {
    let metadata = std::fs::metadata(path).ok()?;
    let mtime = metadata.modified().ok()?;
    let raw = std::fs::read_to_string(path).ok()?;
    let byte_len = raw.len();
    let (content, truncated) = if byte_len > MAX_INSTRUCTIONS_BYTES {
        // Char-boundary-safe truncation. `floor_char_boundary` stabilized
        // in Rust 1.91.0 — matches the crate MSRV pinned in `Cargo.toml`.
        let cut = raw.floor_char_boundary(MAX_INSTRUCTIONS_BYTES);
        let mut clipped = raw[..cut].to_string();
        clipped.push_str(INSTRUCTIONS_TRUNCATION_MARKER);
        (clipped, true)
    } else {
        (raw, false)
    };
    Some(LoadedInstructions {
        path: path.to_path_buf(),
        content,
        mtime,
        byte_len,
        truncated,
    })
}

/// Per-turn auto-reload check. Compares the previously-loaded mtime to
/// the current mtime on disk; reloads only when they differ. The hot
/// path (file unchanged) is one `stat()` syscall — no I/O.
///
/// `cwd` is used to re-discover MERMAID.md when `current` is `None`
/// (handles "user created the file mid-session" by re-running the walk).
pub fn refresh(
    current: Option<LoadedInstructions>,
    cwd: &Path,
) -> (Option<LoadedInstructions>, ReloadOutcome) {
    match current {
        Some(prior) => {
            // Stat the previously-loaded path to detect edits or removal.
            let metadata = std::fs::metadata(&prior.path);
            match metadata.and_then(|m| m.modified()) {
                Ok(new_mtime) if new_mtime == prior.mtime => {
                    // Hot path: no change.
                    (Some(prior), ReloadOutcome::Unchanged)
                },
                Ok(_) => {
                    // mtime changed — re-read.
                    let old_tokens = prior.approx_tokens();
                    let path = prior.path.clone();
                    match load_from_path(&path) {
                        Some(reloaded) => {
                            let new_tokens = reloaded.approx_tokens();
                            (
                                Some(reloaded),
                                ReloadOutcome::Reloaded {
                                    old_tokens,
                                    new_tokens,
                                },
                            )
                        },
                        None => {
                            // mtime moved but read failed (race or
                            // permission) — treat as removed for safety.
                            (None, ReloadOutcome::Removed)
                        },
                    }
                },
                Err(_) => {
                    // File is gone or no longer accessible.
                    (None, ReloadOutcome::Removed)
                },
            }
        },
        None => {
            // No prior load — re-walk in case the user created
            // MERMAID.md after session start.
            match find_mermaid_md(cwd).and_then(|p| load_from_path(&p)) {
                Some(loaded) => {
                    let tokens = loaded.approx_tokens();
                    (Some(loaded), ReloadOutcome::LoadedFirst { tokens })
                },
                None => (None, ReloadOutcome::Unchanged),
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Mutex;

    /// Tests touch the filesystem; serialize them so concurrent test
    /// runs don't see each other's temp files.
    static FS_LOCK: Mutex<()> = Mutex::new(());

    fn temp_dir(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("mermaid_instructions_test_{}", name));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).expect("create temp dir");
        p
    }

    #[test]
    fn find_mermaid_md_finds_in_cwd() {
        let _lock = FS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("cwd");
        fs::write(dir.join("MERMAID.md"), "rules").unwrap();
        let found = find_mermaid_md(&dir).expect("should find");
        assert_eq!(found, dir.join("MERMAID.md"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_mermaid_md_walks_up_to_git_root() {
        let _lock = FS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = temp_dir("walkup");
        fs::create_dir(root.join(".git")).unwrap();
        fs::write(root.join("MERMAID.md"), "root rules").unwrap();
        let sub = root.join("subdir/deeper");
        fs::create_dir_all(&sub).unwrap();
        let found = find_mermaid_md(&sub).expect("should walk up");
        assert_eq!(found, root.join("MERMAID.md"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn find_mermaid_md_stops_at_git_root_without_file() {
        let _lock = FS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = temp_dir("git_no_md");
        fs::create_dir(root.join(".git")).unwrap();
        // Place a MERMAID.md ABOVE the git root — should NOT be found
        // because the walk stops at the .git boundary.
        let parent = root.parent().unwrap();
        let above_md = parent.join("MERMAID.md");
        fs::write(&above_md, "outside").unwrap();
        let sub = root.join("subdir");
        fs::create_dir_all(&sub).unwrap();
        let found = find_mermaid_md(&sub);
        assert!(found.is_none(), "walk must stop at .git boundary");
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_file(&above_md);
    }

    #[test]
    fn find_mermaid_md_returns_none_if_absent() {
        let _lock = FS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("absent");
        // No MERMAID.md anywhere — but also no .git, so the walk
        // continues all the way up. As long as nothing UP the tree
        // happens to have MERMAID.md, this returns None. To make the
        // test deterministic, plant a .git so the walk stops here.
        fs::create_dir(dir.join(".git")).unwrap();
        let found = find_mermaid_md(&dir);
        assert!(found.is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_from_path_truncates_oversized_file() {
        let _lock = FS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("oversized");
        let path = dir.join("MERMAID.md");
        // Write 50 KB — over the 40 KB cap.
        let big = "a".repeat(50_000);
        fs::write(&path, &big).unwrap();
        let loaded = load_from_path(&path).expect("load");
        assert!(loaded.truncated);
        assert_eq!(loaded.byte_len, 50_000); // original size preserved
        assert!(loaded.content.ends_with(INSTRUCTIONS_TRUNCATION_MARKER));
        // Content should be exactly cap + marker length.
        assert_eq!(
            loaded.content.len(),
            MAX_INSTRUCTIONS_BYTES + INSTRUCTIONS_TRUNCATION_MARKER.len()
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_from_path_returns_none_when_missing() {
        let _lock = FS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("missing");
        assert!(load_from_path(&dir.join("nope.md")).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn refresh_returns_unchanged_when_mtime_stable() {
        let _lock = FS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("stable");
        let path = dir.join("MERMAID.md");
        fs::write(&path, "v1").unwrap();
        let prior = load_from_path(&path).unwrap();
        let (after, outcome) = refresh(Some(prior.clone()), &dir);
        assert_eq!(outcome, ReloadOutcome::Unchanged);
        assert!(after.is_some());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn refresh_returns_reloaded_on_content_change() {
        let _lock = FS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("changed");
        let path = dir.join("MERMAID.md");
        fs::write(&path, "v1").unwrap();
        let prior = load_from_path(&path).unwrap();
        // Sleep briefly to ensure mtime resolution registers a change.
        // Most filesystems track mtime at second granularity or finer.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(&path, "v2 longer content here").unwrap();
        let (after, outcome) = refresh(Some(prior), &dir);
        assert!(matches!(outcome, ReloadOutcome::Reloaded { .. }));
        assert_eq!(after.unwrap().content, "v2 longer content here");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn refresh_returns_removed_when_file_deleted() {
        let _lock = FS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("removed");
        let path = dir.join("MERMAID.md");
        fs::write(&path, "v1").unwrap();
        let prior = load_from_path(&path).unwrap();
        fs::remove_file(&path).unwrap();
        let (after, outcome) = refresh(Some(prior), &dir);
        assert_eq!(outcome, ReloadOutcome::Removed);
        assert!(after.is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn refresh_returns_loaded_first_on_initial_discovery() {
        let _lock = FS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("first");
        // Plant .git so the walk stays inside `dir`.
        fs::create_dir(dir.join(".git")).unwrap();
        // No prior load. Call refresh — should discover the new file.
        fs::write(dir.join("MERMAID.md"), "fresh").unwrap();
        let (after, outcome) = refresh(None, &dir);
        assert!(matches!(outcome, ReloadOutcome::LoadedFirst { .. }));
        assert_eq!(after.unwrap().content, "fresh");
        let _ = fs::remove_dir_all(&dir);
    }
}
