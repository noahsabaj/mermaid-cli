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
/// - the user's home directory (`$HOME`, or `%USERPROFILE%` on Windows)
/// - filesystem root
/// - `MAX_WALK_DEPTH` levels (symlink-loop guard)
///
/// Returns all supported instruction files in the nearest matching
/// directory, in precedence order, or an empty vec if none exist.
pub fn find_instruction_files(start: &Path) -> Vec<PathBuf> {
    find_instruction_files_bounded(start, home_dir_boundary().as_deref())
}

/// Resolve the user's home directory to use as the upward-walk boundary.
///
/// On Unix this is `$HOME`. On Windows `HOME` is usually unset — the home var is
/// `%USERPROFILE%` (or `%HOMEDRIVE%%HOMEPATH%`) — so without consulting those the
/// walk would have no home boundary on Windows and could climb above the user's
/// profile, relying solely on `.git` / `MAX_WALK_DEPTH` (F63). An empty value is
/// treated as unset.
fn home_dir_boundary() -> Option<PathBuf> {
    let home = std::env::var_os("HOME");
    #[cfg(windows)]
    let resolved = pick_home_boundary(
        home.as_deref(),
        std::env::var_os("USERPROFILE").as_deref(),
        std::env::var_os("HOMEDRIVE").as_deref(),
        std::env::var_os("HOMEPATH").as_deref(),
    );
    // Non-Windows: only `$HOME` bounds the walk.
    #[cfg(not(windows))]
    let resolved = pick_home_boundary(home.as_deref(), None, None, None);
    resolved
}

/// Pick the home boundary from candidate env values in priority order: `HOME`,
/// then `USERPROFILE`, then `HOMEDRIVE` + `HOMEPATH` (joined). The first present,
/// non-empty value wins; empty values are ignored. Pure (takes its inputs rather
/// than reading the environment) so the Windows fallback order is unit-testable
/// without mutating process-global env (which would race other threads' tests).
fn pick_home_boundary(
    home: Option<&std::ffi::OsStr>,
    userprofile: Option<&std::ffi::OsStr>,
    homedrive: Option<&std::ffi::OsStr>,
    homepath: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    fn nonempty(v: Option<&std::ffi::OsStr>) -> Option<&std::ffi::OsStr> {
        v.filter(|s| !s.is_empty())
    }
    if let Some(home) = nonempty(home) {
        return Some(PathBuf::from(home));
    }
    if let Some(profile) = nonempty(userprofile) {
        return Some(PathBuf::from(profile));
    }
    if let (Some(drive), Some(path)) = (nonempty(homedrive), nonempty(homepath)) {
        let mut combined = drive.to_os_string();
        combined.push(path);
        return Some(PathBuf::from(combined));
    }
    None
}

/// Walk implementation with the `$HOME` boundary injected, so tests can
/// exercise the "stop at home" rule (#108) without mutating the process-global
/// `HOME` env var (which would race other threads' tests).
fn find_instruction_files_bounded(start: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    let mut current = start.to_path_buf();
    for _ in 0..MAX_WALK_DEPTH {
        // Stop at $HOME *before* searching — don't load the user's home-dir
        // instruction files (or anything above home). Checked first so a walk
        // that climbs into home can't pick up `~/AGENTS.md` (#108); the old
        // order searched, found, and returned it before this guard ran.
        if home == Some(current.as_path()) {
            return Vec::new();
        }
        let found: Vec<PathBuf> = INSTRUCTION_FILENAMES
            .iter()
            .map(|name| current.join(name))
            .filter(|candidate| candidate.is_file())
            .collect();
        if !found.is_empty() {
            return found;
        }
        // Stop at the git root (the .git entry itself ends the walk; most
        // projects vendor instruction files at the repo root). Checked *after*
        // discovery so a file AT the git root still loads.
        if current.join(".git").exists() {
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
        // Tolerate a per-file failure: if one path is missing or unreadable
        // (e.g. MERMAID.md removed in the race between `find_instruction_files`
        // and here), skip just that file and load the rest, rather than letting
        // one stat/read failure drop the WHOLE multi-file set (F62). Only when
        // EVERY file fails does the load return `None` (via `sources.first()?`).
        let Ok(metadata) = std::fs::metadata(path) else {
            continue;
        };
        let Ok(mtime) = metadata.modified() else {
            continue;
        };
        let true_len = metadata.len() as usize;
        // Bounded read: never slurp a giant MERMAID.md whole just to truncate it
        // afterwards (#16). Read one byte past the cap so the combined-body
        // truncation check below still detects an oversized single file; the
        // true on-disk size comes from the stat above, so `byte_len` stays
        // accurate rather than reflecting the capped read.
        let Ok((bytes, _truncated)) =
            crate::utils::read_file_capped(path, MAX_INSTRUCTIONS_BYTES.saturating_add(1))
        else {
            continue;
        };
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
    let sections = label_instruction_bodies(bodies);
    let byte_len = total_byte_len;
    let (content, truncated) = combine_and_cap_sections(sections, MAX_INSTRUCTIONS_BYTES);
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

/// Load project instructions + the durable memory index for a one-shot,
/// watcher-less run (`mermaid run` and subagents). The interactive TUI gets
/// these from the config watcher's first poll; the headless and subagent
/// drivers have no watcher, so they must load synchronously before the first
/// model call — otherwise the request goes out with no MERMAID.md/AGENTS.md and
/// no memory index, which is exactly the context `mermaid doctor` reports as
/// loaded.
pub fn load_project_context(
    cwd: &Path,
    mem_cfg: &crate::app::MemoryConfig,
) -> (
    Option<LoadedInstructions>,
    Option<crate::app::memory::LoadedMemory>,
) {
    let (instructions, _) = refresh(None, cwd);
    let (memory, _) = crate::app::memory::refresh(None, cwd, mem_cfg);
    (instructions, memory)
}

/// Separator inserted between labeled instruction sections in the combined body.
const INSTRUCTION_SECTION_SEPARATOR: &str = "\n\n---\n\n";

/// Wrap each `(path, body)` in a labeled header — even a single file — so the
/// content lands in the system prompt as clearly-bounded project data rather
/// than blending into trusted system authority (#109). Returns the labeled
/// sections in load order: lowest precedence first, highest precedence LAST (so
/// `MERMAID.md` lands after `AGENTS.md`).
fn label_instruction_bodies(bodies: Vec<(PathBuf, String)>) -> Vec<String> {
    bodies
        .into_iter()
        .map(|(path, body)| {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("instructions");
            format!("# Project Instructions: {}\n\n{}", name, body)
        })
        .collect()
}

/// Join labeled `sections` into one body capped at `cap` bytes while PRESERVING
/// the documented "MERMAID.md wins on conflict" contract under the cap (F61).
///
/// `sections` is in precedence order, **highest precedence last**. When the
/// combined body fits, it is returned whole. When it overflows, the
/// highest-precedence (last) section — `MERMAID.md` — is protected: the
/// lower-precedence prefix (`AGENTS.md`) is head-truncated to fit, with the
/// truncation marker at the elision point, so the winner survives intact and
/// last (so it still overrides on conflict). Only when the winner *alone*
/// overflows the cap is the winner itself head-clipped (lower-precedence
/// sections dropped). The old code joined `[AGENTS, MERMAID]` then kept the
/// HEAD, so a ≥ 40 KB AGENTS.md silently dropped the entire MERMAID.md tail —
/// letting the lower-priority file win.
///
/// Truncation always lands on a UTF-8 char boundary (`floor_char_boundary`,
/// stabilized in Rust 1.91.0 — matches the crate MSRV in `Cargo.toml`), and the
/// result never exceeds `cap + INSTRUCTIONS_TRUNCATION_MARKER.len()` bytes.
fn combine_and_cap_sections(sections: Vec<String>, cap: usize) -> (String, bool) {
    const SEP: &str = INSTRUCTION_SECTION_SEPARATOR;
    let marker = INSTRUCTIONS_TRUNCATION_MARKER;

    let full = sections.join(SEP);
    if full.len() <= cap {
        return (full, false);
    }
    // Over cap. The winner (highest precedence = MERMAID.md) is the last section
    // and must never be the file that's silently dropped.
    let Some(winner) = sections.last() else {
        return (String::new(), false);
    };
    // Even the winner alone exceeds the cap: there's no room for any
    // lower-precedence content. Drop the rest and head-clip the winner — it
    // still wins, merely truncated.
    if winner.len() >= cap {
        let cut = winner.floor_char_boundary(cap);
        let mut clipped = winner[..cut].to_string();
        clipped.push_str(marker);
        return (clipped, true);
    }
    // The winner fits whole at the tail. Budget the remaining room for the
    // lower-precedence prefix and head-truncate it, so the winner stays intact
    // AND last (so it still overrides on conflict). `earlier_len >= 1` here: a
    // lone section under the cap would have hit the fast path above.
    let earlier_len = sections.len() - 1;
    let earlier_full = sections[..earlier_len].join(SEP);
    let avail = cap.saturating_sub(winner.len() + SEP.len());
    let cut = earlier_full.floor_char_boundary(avail);
    if cut == 0 {
        // No lower-precedence content survives the budget: keep the winner whole
        // (dropping the rest) with a trailing marker so the elision is visible.
        let mut body = winner.clone();
        body.push_str(marker);
        return (body, true);
    }
    let mut body = String::with_capacity(cut + marker.len() + SEP.len() + winner.len());
    body.push_str(&earlier_full[..cut]);
    body.push_str(marker);
    body.push_str(SEP);
    body.push_str(winner.as_str());
    (body, true)
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
    fn find_instruction_files_stops_at_home_boundary() {
        // #108: a walk that climbs into $HOME must NOT pick up the home-dir
        // AGENTS.md. The boundary is injected (no global env mutation), so the
        // test is race-free. Without the fix the walk would search `home`,
        // find AGENTS.md, and return it before the home guard ever ran.
        let _lock = FS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = temp_dir("home_boundary");
        fs::write(home.join("AGENTS.md"), "home rules").unwrap();
        let child = home.join("project");
        fs::create_dir_all(&child).unwrap();
        // No .git anywhere between child and home, so only the home guard can
        // stop the climb.
        let found = find_instruction_files_bounded(&child, Some(home.as_path()));
        assert!(
            found.is_empty(),
            "walk must stop at $HOME and not load ~/AGENTS.md, got {found:?}"
        );
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn single_file_instructions_get_labeled_header() {
        // #109: even a single instruction file is wrapped in a labeled
        // boundary so it reaches the system prompt as clearly-bounded project
        // data, not unlabeled trusted-system text.
        let _lock = FS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("single_header");
        fs::write(dir.join("MERMAID.md"), "do the thing").unwrap();
        let loaded = load_from_path(&dir.join("MERMAID.md")).expect("load");
        assert!(
            loaded
                .content
                .starts_with("# Project Instructions: MERMAID.md"),
            "single-file instructions must carry a labeled header, got: {:?}",
            loaded.content
        );
        assert!(loaded.content.contains("do the thing"));
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
        let content = after.unwrap().content;
        assert!(content.contains("# Project Instructions: MERMAID.md"));
        assert!(content.contains("v2 longer content here"));
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
        let content = after.unwrap().content;
        assert!(content.contains("# Project Instructions: MERMAID.md"));
        assert!(content.contains("fresh"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_project_context_loads_instructions_synchronously() {
        // The one-shot paths (headless `mermaid run`, subagents) call this
        // instead of relying on the config watcher, so it must surface MERMAID.md
        // immediately — the bug was that headless runs saw no instructions at all.
        let _lock = FS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("project_context");
        fs::create_dir(dir.join(".git")).unwrap();
        fs::write(dir.join("MERMAID.md"), "sync-loaded instructions").unwrap();
        let (instructions, _memory) =
            load_project_context(&dir, &crate::app::MemoryConfig::default());
        let content = instructions
            .expect("instructions must load synchronously")
            .content;
        assert!(content.contains("sync-loaded instructions"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn oversized_agents_does_not_drop_mermaid_winner() {
        // F61: when AGENTS.md alone is huge, head-truncating the COMBINED body
        // used to drop the entire MERMAID.md tail — silently letting the
        // lower-priority file "win". MERMAID.md must survive intact and last (so
        // it still overrides on conflict); AGENTS.md is the file truncated.
        let _lock = FS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("agents_huge");
        fs::write(dir.join("AGENTS.md"), "A".repeat(60_000)).unwrap();
        fs::write(dir.join("MERMAID.md"), "MERMAID_WINS_SENTINEL").unwrap();
        let loaded =
            load_from_paths(&[dir.join("AGENTS.md"), dir.join("MERMAID.md")]).expect("load");
        assert!(loaded.truncated, "combined body exceeds the cap");
        assert!(
            loaded.content.contains("MERMAID_WINS_SENTINEL"),
            "MERMAID.md (the winner) must survive the cap, not be dropped"
        );
        assert!(
            loaded
                .content
                .contains("# Project Instructions: MERMAID.md")
        );
        assert!(loaded.content.contains(INSTRUCTIONS_TRUNCATION_MARKER));
        // The winner lands AFTER the elision marker — AGENTS.md was the file
        // truncated, and MERMAID.md still comes last so it overrides on conflict.
        let marker_at = loaded.content.find(INSTRUCTIONS_TRUNCATION_MARKER).unwrap();
        let winner_at = loaded.content.find("MERMAID_WINS_SENTINEL").unwrap();
        assert!(
            winner_at > marker_at,
            "MERMAID.md must come after the truncated AGENTS.md"
        );
        // Bounded: never more than the cap plus a single marker.
        assert!(
            loaded.content.len() <= MAX_INSTRUCTIONS_BYTES + INSTRUCTIONS_TRUNCATION_MARKER.len()
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn combine_and_cap_protects_the_last_section() {
        // Lower-precedence section is large; the small winner must survive whole
        // and land last, with the marker at the elision point.
        let lower = format!("# Project Instructions: AGENTS.md\n\n{}", "L".repeat(200));
        let winner = "# Project Instructions: MERMAID.md\n\nWIN".to_string();
        let (body, truncated) = combine_and_cap_sections(vec![lower, winner], 100);
        assert!(truncated);
        assert!(body.contains("WIN"), "winner survives the cap");
        assert!(body.contains(INSTRUCTIONS_TRUNCATION_MARKER));
        let marker_at = body.find(INSTRUCTIONS_TRUNCATION_MARKER).unwrap();
        assert!(
            body.find("WIN").unwrap() > marker_at,
            "winner stays last so it overrides on conflict"
        );
        assert!(body.len() <= 100 + INSTRUCTIONS_TRUNCATION_MARKER.len());
    }

    #[test]
    fn combine_and_cap_clips_winner_when_it_alone_overflows() {
        // When even the winner exceeds the cap, it's head-clipped (not dropped)
        // and the lower-precedence section is dropped entirely.
        let lower = "# Project Instructions: AGENTS.md\n\nlower-content".to_string();
        let winner = format!("# Project Instructions: MERMAID.md\n\n{}", "W".repeat(300));
        let (body, truncated) = combine_and_cap_sections(vec![lower, winner], 100);
        assert!(truncated);
        assert!(body.ends_with(INSTRUCTIONS_TRUNCATION_MARKER));
        assert_eq!(body.len(), 100 + INSTRUCTIONS_TRUNCATION_MARKER.len());
        assert!(
            !body.contains("lower-content"),
            "no room for the lower-precedence section"
        );
    }

    #[test]
    fn combine_and_cap_passes_through_when_it_fits() {
        let a = "# Project Instructions: AGENTS.md\n\naye".to_string();
        let b = "# Project Instructions: MERMAID.md\n\nbee".to_string();
        let (body, truncated) = combine_and_cap_sections(vec![a, b], 10_000);
        assert!(!truncated);
        assert!(body.contains("aye") && body.contains("bee"));
        assert!(
            body.find("bee").unwrap() > body.find("aye").unwrap(),
            "highest-precedence section stays last"
        );
    }

    #[test]
    fn load_from_paths_tolerates_a_missing_file() {
        // F62: if one path is missing (e.g. MERMAID.md removed in the race
        // between discovery and load), the present file(s) must still load
        // rather than the whole multi-file set returning None.
        let _lock = FS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("partial_load");
        fs::write(dir.join("AGENTS.md"), "agent rules").unwrap();
        let missing = dir.join("MERMAID.md"); // never created
        let loaded = load_from_paths(&[dir.join("AGENTS.md"), missing])
            .expect("AGENTS.md must still load when MERMAID.md is absent");
        assert!(loaded.content.contains("# Project Instructions: AGENTS.md"));
        assert!(loaded.content.contains("agent rules"));
        assert_eq!(loaded.sources.len(), 1, "only the present file is a source");
        assert_eq!(loaded.path, dir.join("AGENTS.md"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_from_paths_none_when_all_missing() {
        let _lock = FS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("all_missing");
        assert!(
            load_from_paths(&[dir.join("AGENTS.md"), dir.join("MERMAID.md")]).is_none(),
            "no present files => None"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn pick_home_boundary_resolves_windows_home_vars() {
        // F63: on Windows `HOME` is usually unset; the home boundary must fall
        // back to `%USERPROFILE%`, then `%HOMEDRIVE%%HOMEPATH%`. Exercised here
        // with synthetic values so it's verifiable on every platform.
        use std::ffi::OsStr;
        // HOME wins when present.
        assert_eq!(
            pick_home_boundary(
                Some(OsStr::new("/home/me")),
                Some(OsStr::new("C:\\Users\\me")),
                None,
                None
            ),
            Some(PathBuf::from("/home/me"))
        );
        // No HOME => USERPROFILE (the Windows home var) bounds the walk.
        assert_eq!(
            pick_home_boundary(None, Some(OsStr::new("C:\\Users\\me")), None, None),
            Some(PathBuf::from("C:\\Users\\me"))
        );
        // No HOME/USERPROFILE => HOMEDRIVE + HOMEPATH joined.
        assert_eq!(
            pick_home_boundary(
                None,
                None,
                Some(OsStr::new("C:")),
                Some(OsStr::new("\\Users\\me"))
            ),
            Some(PathBuf::from("C:\\Users\\me"))
        );
        // Empty values are ignored (not a usable boundary).
        assert_eq!(
            pick_home_boundary(Some(OsStr::new("")), Some(OsStr::new("")), None, None),
            None
        );
        // HOMEDRIVE without HOMEPATH (and vice-versa) => no boundary.
        assert_eq!(
            pick_home_boundary(None, None, Some(OsStr::new("C:")), None),
            None
        );
        assert_eq!(
            pick_home_boundary(None, None, None, Some(OsStr::new("\\Users\\me"))),
            None
        );
    }
}
