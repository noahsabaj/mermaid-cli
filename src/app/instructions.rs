//! Project-instructions loader (Step 5h).
//!
//! On session start, walk UP from the current working directory looking
//! for repo instruction files. Stop at the git root (any directory
//! containing a `.git` entry) or at `$HOME`, whichever is reached first.
//! Load every supported file in the nearest matching directory; cap the
//! combined body at `MAX_INSTRUCTIONS_BYTES`; pass the content to the
//! model as a dynamic suffix on the system prompt.
//!
//! Auto-reload: before every model call, `refresh()` stats the loaded
//! file's path and compares mtime. If the mtime moved, re-read; if the
//! file is gone, drop the instructions. One stat per turn is
//! microseconds — no need for a filesystem watcher.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::constants::{INSTRUCTIONS_TRUNCATION_MARKER, MAX_INSTRUCTIONS_BYTES};

/// Instruction files Mermaid understands, in load order. `AGENTS.md` (the
/// cross-tool open standard) is read first; `MERMAID.md` (mermaid-specific) is
/// read last so its guidance overrides `AGENTS.md` on conflict. These are the
/// only two recognized — there is intentionally no CLAUDE.md/GEMINI.md support.
pub const INSTRUCTION_FILENAMES: &[&str] = &["AGENTS.md", "MERMAID.md"];

/// Hard cap on how many directory levels `find_instruction_files` walks up
/// before giving up. Guards against pathological symlink loops.
const MAX_WALK_DEPTH: usize = 32;

/// One loaded instruction file inside a combined project-instructions
/// snapshot.
#[derive(Debug, Clone)]
pub struct InstructionSource {
    pub path: PathBuf,
    pub mtime: SystemTime,
    pub byte_len: usize,
}

/// One-shot snapshot of loaded project instructions. Stored on `App` and
/// `NonInteractiveRunner` so the per-turn auto-reload check has
/// something to compare against.
#[derive(Debug, Clone)]
pub struct LoadedInstructions {
    /// Primary absolute path the content was read from. Kept for
    /// compatibility with older renderer/status code; `sources`
    /// carries the full set.
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
    /// All files that contributed to `content`.
    pub sources: Vec<InstructionSource>,
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

/// Walk UP from `start` looking for any supported instruction file.
/// Stops at the first of:
/// - a directory containing `.git` (the git root)
/// - `$HOME` (don't search above the user's home)
/// - filesystem root
/// - `MAX_WALK_DEPTH` levels (symlink-loop guard)
///
/// Returns all supported instruction files in the nearest matching
/// directory, in precedence order, or an empty vec if none exist.
pub fn find_instruction_files(start: &Path) -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut current = start.to_path_buf();
    for _ in 0..MAX_WALK_DEPTH {
        let found: Vec<PathBuf> = INSTRUCTION_FILENAMES
            .iter()
            .map(|name| current.join(name))
            .filter(|candidate| candidate.is_file())
            .collect();
        if !found.is_empty() {
            return found;
        }
        // Stop at the git root (the .git entry itself ends the walk;
        // most projects vendor instruction files at the repo root).
        if current.join(".git").exists() {
            return Vec::new();
        }
        // Stop at $HOME — don't search the user's home directory or
        // anything above it. Avoids accidentally picking up a
        // long-forgotten instruction file from a sibling project.
        if let Some(ref h) = home
            && current == *h
        {
            return Vec::new();
        }
        // Move up one level. If we're at the filesystem root, stop.
        match current.parent() {
            Some(parent) if parent != current => current = parent.to_path_buf(),
            _ => return Vec::new(),
        }
    }
    Vec::new()
}

/// Read the file at `path`, truncate to `MAX_INSTRUCTIONS_BYTES` if
/// oversized, and return a `LoadedInstructions`. Returns `None` if the
/// file can't be read or doesn't exist.
pub fn load_from_path(path: &Path) -> Option<LoadedInstructions> {
    load_from_paths(&[path.to_path_buf()])
}

/// Read and combine the instruction files at `paths`, truncating the
/// combined body to `MAX_INSTRUCTIONS_BYTES` if needed.
pub fn load_from_paths(paths: &[PathBuf]) -> Option<LoadedInstructions> {
    let mut sources = Vec::new();
    let mut bodies = Vec::new();
    let mut total_byte_len = 0usize;
    let mut latest_mtime = UNIX_EPOCH;

    for path in paths {
        let metadata = std::fs::metadata(path).ok()?;
        let mtime = metadata.modified().ok()?;
        let true_len = metadata.len() as usize;
        // Bounded read: never slurp a giant MERMAID.md whole just to truncate it
        // afterwards (#16). Read one byte past the cap so the combined-body
        // truncation check below still detects an oversized single file; the
        // true on-disk size comes from the stat above, so `byte_len` stays
        // accurate rather than reflecting the capped read.
        let (bytes, _truncated) =
            crate::utils::read_file_capped(path, MAX_INSTRUCTIONS_BYTES.saturating_add(1)).ok()?;
        let raw = String::from_utf8_lossy(&bytes).into_owned();
        total_byte_len = total_byte_len.saturating_add(true_len);
        if mtime > latest_mtime {
            latest_mtime = mtime;
        }
        sources.push(InstructionSource {
            path: path.to_path_buf(),
            mtime,
            byte_len: true_len,
        });
        bodies.push((path.to_path_buf(), raw));
    }
    let primary = sources.first()?.path.clone();
    let raw = combine_instruction_bodies(bodies);
    let byte_len = total_byte_len;
    let (content, truncated) = if raw.len() > MAX_INSTRUCTIONS_BYTES {
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
        path: primary,
        content,
        mtime: latest_mtime,
        byte_len,
        truncated,
        sources,
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
            let paths: Vec<PathBuf> = if prior.sources.is_empty() {
                vec![prior.path.clone()]
            } else {
                prior
                    .sources
                    .iter()
                    .map(|source| source.path.clone())
                    .collect()
            };
            let changed = if prior.sources.is_empty() {
                std::fs::metadata(&prior.path)
                    .and_then(|m| m.modified())
                    .map(|mtime| mtime != prior.mtime)
                    .unwrap_or(true)
            } else {
                prior.sources.iter().any(|source| {
                    std::fs::metadata(&source.path)
                        .and_then(|m| m.modified())
                        .map(|mtime| mtime != source.mtime)
                        .unwrap_or(true)
                })
            };
            if !changed {
                return (Some(prior), ReloadOutcome::Unchanged);
            }
            let old_tokens = prior.approx_tokens();
            match load_from_paths(&paths) {
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
                    // mtime moved but read failed (race or permission)
                    // — treat as removed for safety.
                    (None, ReloadOutcome::Removed)
                },
            }
        },
        None => {
            // No prior load — re-walk in case the user created
            // instruction files after session start.
            match load_from_paths(&find_instruction_files(cwd)) {
                Some(loaded) => {
                    let tokens = loaded.approx_tokens();
                    (Some(loaded), ReloadOutcome::LoadedFirst { tokens })
                },
                None => (None, ReloadOutcome::Unchanged),
            }
        },
    }
}

fn combine_instruction_bodies(bodies: Vec<(PathBuf, String)>) -> String {
    if bodies.len() == 1 {
        return bodies
            .into_iter()
            .next()
            .map(|(_, body)| body)
            .unwrap_or_default();
    }
    bodies
        .into_iter()
        .map(|(path, body)| {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("instructions");
            format!("# Project Instructions: {}\n\n{}", name, body)
        })
        .collect::<Vec<_>>()
        .join("\n\n---\n\n")
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
    fn find_instruction_files_finds_in_cwd() {
        let _lock = FS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("cwd");
        fs::write(dir.join("MERMAID.md"), "rules").unwrap();
        let found = find_instruction_files(&dir);
        assert_eq!(found, vec![dir.join("MERMAID.md")]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_instruction_files_loads_both_in_precedence_order() {
        let _lock = FS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("both");
        fs::write(dir.join("AGENTS.md"), "agent rules").unwrap();
        fs::write(dir.join("MERMAID.md"), "mermaid rules").unwrap();
        let found = find_instruction_files(&dir);
        // AGENTS.md first, MERMAID.md last (last wins on conflict).
        assert_eq!(found, vec![dir.join("AGENTS.md"), dir.join("MERMAID.md")]);
        let loaded = load_from_paths(&found).expect("load combined");
        assert!(loaded.content.contains("# Project Instructions: AGENTS.md"));
        assert!(loaded.content.contains("agent rules"));
        assert!(
            loaded
                .content
                .contains("# Project Instructions: MERMAID.md")
        );
        assert!(loaded.content.contains("mermaid rules"));
        // MERMAID.md body must appear AFTER AGENTS.md so it overrides.
        assert!(
            loaded.content.find("mermaid rules") > loaded.content.find("agent rules"),
            "MERMAID.md must come last so its guidance overrides AGENTS.md"
        );
        assert_eq!(loaded.sources.len(), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_instruction_files_walks_up_to_git_root() {
        let _lock = FS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = temp_dir("walkup");
        fs::create_dir(root.join(".git")).unwrap();
        fs::write(root.join("MERMAID.md"), "root rules").unwrap();
        let sub = root.join("subdir/deeper");
        fs::create_dir_all(&sub).unwrap();
        let found = find_instruction_files(&sub);
        assert_eq!(found, vec![root.join("MERMAID.md")]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn find_instruction_files_stops_at_git_root_without_file() {
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
        let found = find_instruction_files(&sub);
        assert!(found.is_empty(), "walk must stop at .git boundary");
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_file(&above_md);
    }

    #[test]
    fn find_instruction_files_returns_empty_if_absent() {
        let _lock = FS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("absent");
        // No instruction file anywhere. Plant a .git so the walk stops
        // here deterministically rather than climbing the real tree.
        fs::create_dir(dir.join(".git")).unwrap();
        let found = find_instruction_files(&dir);
        assert!(found.is_empty());
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
