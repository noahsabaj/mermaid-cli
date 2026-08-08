//! Durable semantic memory (v0.10.0).
//!
//! Plain-Markdown, agent-managed long-term memory: one fact per file with
//! YAML frontmatter (`name`, `description`, `scope`, `created`, `tags`) and a
//! body. Three scopes, all machine-local except shared:
//!   - **global**     `<data_dir>/memory/`                       (all projects)
//!   - **project-private** `<data_dir>/projects/<key>/memory/`   (default; not committed)
//!   - **project-shared**  `<git-root>/.mermaid/memory/`         (opt-in; committed)
//!
//! Retrieval is an always-loaded auto-derived INDEX (name + description + path
//! per file, grouped by scope) plus on-demand reads of the full files via the
//! normal `read_file` tool. The index is generated from the files, so it can
//! never drift from them. No database, no vectors, no embeddings.
//!
//! This module owns the on-disk format, scope resolution, index generation,
//! load/refresh, and the write/delete primitives the memory tool and slash
//! commands build on.

use mermaid_domain::{LoadedMemory, MemoryEntry, MemoryScope};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use mermaid_domain::MemoryConfig;
use mermaid_model::constants::MEMORY_INDEX_TRUNCATION_MARKER;

/// Hard cap on directory levels `find_git_root` walks up (symlink-loop guard).
const MAX_WALK_DEPTH: usize = 32;

/// Per-file byte cap when reading a memory `.md` during the per-turn index
/// refresh. A single fact is tiny; this only bounds a pathological/huge file so
/// `refresh()` can't be made to slurp unbounded bytes every turn (F47).
const MAX_MEMORY_FILE_BYTES: usize = 64_000;

/// Outcome of a per-turn `refresh()`, for optional status reporting.
#[derive(Debug, PartialEq, Eq)]
pub enum MemoryReloadOutcome {
    Unchanged,
    Reloaded,
    LoadedFirst,
    Removed,
}

/// Walk UP from `start` to the nearest directory containing a `.git` entry
/// (file or dir, so worktrees resolve), or `None` if not inside a repo.
pub fn find_git_root(start: &Path) -> Option<PathBuf> {
    let mut current = start.to_path_buf();
    for _ in 0..MAX_WALK_DEPTH {
        if current.join(".git").exists() {
            return Some(current);
        }
        match current.parent() {
            Some(parent) if parent != current => current = parent.to_path_buf(),
            _ => return None,
        }
    }
    None
}

/// The memory roots for `cwd`, in injection order (global → private → shared).
/// Shared is omitted when `cwd` isn't in a git repo. Returns an empty vec only
/// if the machine data dir can't be resolved.
pub fn memory_roots(cwd: &Path) -> Vec<(PathBuf, MemoryScope)> {
    let Ok(data) = mermaid_runtime::data_dir() else {
        return Vec::new();
    };
    let mut roots = vec![(data.join("memory"), MemoryScope::Global)];
    match find_git_root(cwd) {
        Some(git_root) => {
            roots.push((
                data.join("projects")
                    .join(project_key(&git_root))
                    .join("memory"),
                MemoryScope::ProjectPrivate,
            ));
            roots.push((
                git_root.join(".mermaid").join("memory"),
                MemoryScope::ProjectShared,
            ));
        },
        None => {
            // Not a repo: key private memory off the canonical cwd; no shared.
            roots.push((
                data.join("projects").join(project_key(cwd)).join("memory"),
                MemoryScope::ProjectPrivate,
            ));
        },
    }
    roots
}

/// Resolve the on-disk directory for a scope at `cwd`, if available.
pub fn dir_for(scope: MemoryScope, cwd: &Path) -> Option<PathBuf> {
    memory_roots(cwd)
        .into_iter()
        .find(|(_, s)| *s == scope)
        .map(|(dir, _)| dir)
}

/// Stable machine-local key for a project path: `<slug>-<hash8>`. The slug is
/// human-debuggable; the hash disambiguates same-named dirs. Only keys the
/// machine-local private store, so cross-machine stability is irrelevant.
fn project_key(path: &Path) -> String {
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let slug: String = canonical
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .take(32)
        .collect();
    let slug = slug.trim_matches('-');
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    canonical.to_string_lossy().hash(&mut hasher);
    let hash = hasher.finish() as u32;
    let slug = if slug.is_empty() { "project" } else { slug };
    format!("{slug}-{hash:08x}")
}

/// kebab-case slug for a memory name → filename stem.
pub fn slugify(name: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in name.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    let slug = out.trim_matches('-').to_string();
    if slug.is_empty() {
        "memory".to_string()
    } else {
        slug
    }
}

#[derive(Debug, Default)]
struct Frontmatter {
    name: Option<String>,
    description: Option<String>,
}

/// Split a memory file into its (name, description) frontmatter and body. A
/// file without a leading `---` fence is treated as all-body. Tolerant: a
/// malformed/unclosed fence falls back to the whole content as body.
fn parse_frontmatter(raw: &str) -> (Frontmatter, String) {
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let mut lines = raw.lines();
    if lines.next().map(str::trim) != Some("---") {
        return (Frontmatter::default(), raw.to_string());
    }
    let mut fm = Frontmatter::default();
    let mut body_lines: Vec<&str> = Vec::new();
    let mut in_fm = true;
    for line in lines {
        if in_fm {
            if line.trim() == "---" {
                in_fm = false;
                continue;
            }
            if let Some((key, value)) = line.split_once(':') {
                let value = value.trim().trim_matches('"').to_string();
                match key.trim() {
                    "name" => fm.name = Some(value),
                    "description" => fm.description = Some(value),
                    _ => {},
                }
            }
        } else {
            body_lines.push(line);
        }
    }
    if in_fm {
        // Unclosed fence — not real frontmatter.
        return (Frontmatter::default(), raw.to_string());
    }
    (fm, body_lines.join("\n").trim().to_string())
}

/// Load all `*.md` memories in `dir` (non-recursive) as index entries. Missing
/// dir ⇒ empty. Sorted by name for a deterministic index.
fn load_root(dir: &Path, scope: MemoryScope) -> Vec<MemoryEntry> {
    let mut entries = Vec::new();
    let Ok(read) = std::fs::read_dir(dir) else {
        return entries;
    };
    for entry in read.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let mtime = meta.modified().unwrap_or(UNIX_EPOCH);
        // Bounded read: this dir is re-scanned every turn by `refresh()`, so
        // never slurp a pathologically large `.md` whole — and surface (not
        // silently swallow) a read error instead of indexing an empty stub (F47).
        let raw = match mermaid_model::utils::read_file_capped(&path, MAX_MEMORY_FILE_BYTES) {
            Ok((bytes, _truncated)) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "memory: skipping unreadable file");
                continue;
            },
        };
        let (fm, body) = parse_frontmatter(&raw);
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("memory")
            .to_string();
        let name = fm.name.filter(|s| !s.is_empty()).unwrap_or(stem);
        let description = fm.description.filter(|s| !s.is_empty()).unwrap_or_else(|| {
            body.lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .to_string()
        });
        entries.push(MemoryEntry {
            name,
            description,
            path,
            scope,
            mtime,
        });
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
}

/// Render the always-loaded index from entries, grouped global → private →
/// shared, clipped to `cap` bytes with a marker if oversized.
fn render_index(entries: &[MemoryEntry], cap: usize) -> (String, bool) {
    if entries.is_empty() {
        return (String::new(), false);
    }
    let mut out = String::from(
        "# Memory\n\nDurable facts you have saved across sessions. Read a file with `read_file` when its description is relevant; change memory with the `memory` tool.\n",
    );
    for scope in [
        MemoryScope::Global,
        MemoryScope::ProjectPrivate,
        MemoryScope::ProjectShared,
    ] {
        let mut first = true;
        for entry in entries.iter().filter(|e| e.scope == scope) {
            if first {
                out.push_str(&format!("\n## {}\n", scope.label()));
                first = false;
            }
            out.push_str(&format!(
                "- [{}] {} — {}\n",
                entry.name,
                entry.description,
                entry.path.display()
            ));
        }
    }
    if out.len() > cap {
        let cut = out.floor_char_boundary(cap);
        let mut clipped = out[..cut].to_string();
        clipped.push_str(MEMORY_INDEX_TRUNCATION_MARKER);
        (clipped, true)
    } else {
        (out, false)
    }
}

/// Load all memory for `cwd` into a snapshot, or `None` when disabled or empty.
pub fn load(cwd: &Path, cfg: &MemoryConfig) -> Option<LoadedMemory> {
    if !cfg.enabled {
        return None;
    }
    let mut entries = Vec::new();
    for (dir, scope) in memory_roots(cwd) {
        entries.extend(load_root(&dir, scope));
    }
    if entries.is_empty() {
        return None;
    }
    let (index, truncated) = render_index(&entries, cfg.index_cap_bytes);
    Some(LoadedMemory {
        entries,
        index,
        truncated,
    })
}

/// Per-turn refresh: re-scan the roots (cheap — a few `read_dir`s + stats) and
/// report whether anything changed since `current`. Picks up the agent's own
/// mid-session writes and hand edits with no filesystem watcher.
pub fn refresh(
    current: Option<LoadedMemory>,
    cwd: &Path,
    cfg: &MemoryConfig,
) -> (Option<LoadedMemory>, MemoryReloadOutcome) {
    let fresh = load(cwd, cfg);
    let outcome = match (&current, &fresh) {
        (None, None) => MemoryReloadOutcome::Unchanged,
        (None, Some(_)) => MemoryReloadOutcome::LoadedFirst,
        (Some(_), None) => MemoryReloadOutcome::Removed,
        (Some(prev), Some(next)) => {
            if same_entries(prev, next) {
                MemoryReloadOutcome::Unchanged
            } else {
                MemoryReloadOutcome::Reloaded
            }
        },
    };
    (fresh, outcome)
}

fn same_entries(a: &LoadedMemory, b: &LoadedMemory) -> bool {
    a.entries.len() == b.entries.len()
        && a.entries
            .iter()
            .zip(&b.entries)
            .all(|(x, y)| x.path == y.path && x.mtime == y.mtime)
}

/// Render a memory file's content (frontmatter + body).
fn render_file(
    name: &str,
    description: &str,
    scope: MemoryScope,
    tags: &[String],
    body: &str,
) -> String {
    let created = chrono::Utc::now().to_rfc3339();
    let tags = tags
        .iter()
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "---\nname: {name}\ndescription: {description}\nscope: {scope}\ncreated: {created}\ntags: [{tags}]\n---\n\n{body}\n",
        scope = scope.as_str(),
        body = body.trim_end(),
    )
}

/// Write a memory into `dir` (created if needed). Returns the file path.
/// Testable core of `write_memory`.
pub fn write_to_dir(
    dir: &Path,
    name: &str,
    description: &str,
    scope: MemoryScope,
    tags: &[String],
    body: &str,
) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    // Redact credential-shaped strings before persisting model-written memory:
    // a fact that summarizes a `.env` the model read would otherwise store a
    // key in the durable (and always-index-loaded) memory file (#69). Scrub all
    // four fields — the `name` re-enters the always-loaded system-prompt index
    // and `tags` ride along in frontmatter, so redacting only description+body
    // would still leak a secret pasted into the name/tags (F9). Redact the name
    // BEFORE slugifying so a credential can't survive in the on-disk filename.
    let name = mermaid_model::utils::redact_secrets(name);
    let description = mermaid_model::utils::redact_secrets(description);
    let tags: Vec<String> = tags
        .iter()
        .map(|t| mermaid_model::utils::redact_secrets(t))
        .collect();
    let body = mermaid_model::utils::redact_secrets(body);
    let path = dir.join(format!("{}.md", slugify(&name)));
    std::fs::write(&path, render_file(&name, &description, scope, &tags, &body))?;
    Ok(path)
}

/// Write a memory at the resolved directory for `scope`/`cwd`.
pub fn write_memory(
    cwd: &Path,
    scope: MemoryScope,
    name: &str,
    description: &str,
    tags: &[String],
    body: &str,
) -> std::io::Result<PathBuf> {
    let dir = dir_for(scope, cwd).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "could not resolve a memory directory",
        )
    })?;
    write_to_dir(&dir, name, description, scope, tags, body)
}

/// Find a memory by name or file-stem id across all scopes.
pub fn find(cwd: &Path, id_or_name: &str) -> Option<MemoryEntry> {
    for (dir, scope) in memory_roots(cwd) {
        for entry in load_root(&dir, scope) {
            let stem = entry.path.file_stem().and_then(|s| s.to_str());
            if entry.name == id_or_name || stem == Some(id_or_name) {
                return Some(entry);
            }
        }
    }
    None
}

/// Delete a memory by name or file-stem id. Returns the deleted path, or
/// `None` if no match.
pub fn delete_memory(cwd: &Path, id_or_name: &str) -> std::io::Result<Option<PathBuf>> {
    match find(cwd, id_or_name) {
        Some(entry) => {
            std::fs::remove_file(&entry.path)?;
            Ok(Some(entry.path))
        },
        None => Ok(None),
    }
}

/// Load every memory's index entry paired with its full body text, across all
/// scopes. Consolidation needs the bodies to judge duplicates/staleness.
pub fn entries_with_bodies(cwd: &Path) -> Vec<(MemoryEntry, String)> {
    let mut out = Vec::new();
    for (dir, scope) in memory_roots(cwd) {
        for entry in load_root(&dir, scope) {
            let raw = std::fs::read_to_string(&entry.path).unwrap_or_default();
            let (_, body) = parse_frontmatter(&raw);
            out.push((entry, body));
        }
    }
    out
}

/// One hit from a memory search: the matching entry plus a short excerpt of the
/// line where the query matched (falling back to the description when the match
/// is in the name/description rather than the body).
#[derive(Debug, Clone)]
pub struct MemorySearchHit {
    pub entry: MemoryEntry,
    pub snippet: String,
}

/// Search all memory across scopes for `query` — a case-insensitive substring
/// match over each fact's name, description, and body. No embeddings or vectors
/// (matches Mermaid's stated stance); a plain scan over the already-bounded
/// memory corpus. Bodies on disk are redacted at write time, so snippets are
/// safe to surface. Returns an empty vec for a blank query.
pub fn search(cwd: &Path, query: &str) -> Vec<MemorySearchHit> {
    search_entries(entries_with_bodies(cwd), query)
}

/// Core matcher for [`search`], split out so it can be tested over hand-built
/// entries without touching the real per-user memory directories.
fn search_entries(entries: Vec<(MemoryEntry, String)>, query: &str) -> Vec<MemorySearchHit> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (entry, body) in entries {
        let body_line = body
            .lines()
            .find(|line| line.to_lowercase().contains(&needle));
        let matches = entry.name.to_lowercase().contains(&needle)
            || entry.description.to_lowercase().contains(&needle)
            || body_line.is_some();
        if !matches {
            continue;
        }
        let raw_snippet = body_line
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .unwrap_or(entry.description.as_str());
        let snippet = clip_chars(raw_snippet, 160);
        out.push(MemorySearchHit { entry, snippet });
    }
    out
}

/// Clip `s` to at most `max` characters on a char boundary, appending a single
/// ellipsis when truncated. Used for search snippets.
fn clip_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let clipped: String = s.chars().take(max).collect();
    format!("{clipped}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use mermaid_model::constants::MAX_MEMORY_INDEX_BYTES;
    use std::fs;
    use std::sync::Mutex;
    use std::time::SystemTime;

    static FS_LOCK: Mutex<()> = Mutex::new(());

    fn temp_dir(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("mermaid_memory_test_{name}"));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).expect("create temp dir");
        p
    }

    #[test]
    fn slugify_makes_safe_stems() {
        assert_eq!(slugify("Prefer ripgrep!"), "prefer-ripgrep");
        assert_eq!(slugify("  use   pnpm  "), "use-pnpm");
        assert_eq!(slugify("***"), "memory");
    }

    #[test]
    fn parse_frontmatter_extracts_name_and_description() {
        let raw =
            "---\nname: prefer-rg\ndescription: Use ripgrep\ntags: [tooling]\n---\n\nThe body.\n";
        let (fm, body) = parse_frontmatter(raw);
        assert_eq!(fm.name.as_deref(), Some("prefer-rg"));
        assert_eq!(fm.description.as_deref(), Some("Use ripgrep"));
        assert_eq!(body, "The body.");
    }

    #[test]
    fn parse_frontmatter_without_fence_is_all_body() {
        let (fm, body) = parse_frontmatter("just a note\nsecond line");
        assert!(fm.name.is_none());
        assert_eq!(body, "just a note\nsecond line");
    }

    #[test]
    fn render_and_parse_round_trip() {
        let content = render_file(
            "prefer-rg",
            "Use ripgrep",
            MemoryScope::ProjectShared,
            &["tooling".to_string()],
            "Always reach for rg.",
        );
        assert!(content.contains("scope: project-shared"));
        let (fm, body) = parse_frontmatter(&content);
        assert_eq!(fm.name.as_deref(), Some("prefer-rg"));
        assert_eq!(fm.description.as_deref(), Some("Use ripgrep"));
        assert_eq!(body, "Always reach for rg.");
    }

    #[test]
    fn write_and_load_root_round_trip() {
        let _lock = FS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("root");
        write_to_dir(
            &dir,
            "Test Fact",
            "A description",
            MemoryScope::Global,
            &[],
            "body",
        )
        .unwrap();
        let entries = load_root(&dir, MemoryScope::Global);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "Test Fact");
        assert_eq!(entries[0].description, "A description");
        assert_eq!(entries[0].path.file_name().unwrap(), "test-fact.md");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_entries_matches_name_description_and_body() {
        let mk = |name: &str, desc: &str| MemoryEntry {
            name: name.to_string(),
            description: desc.to_string(),
            path: PathBuf::from(format!("/tmp/{name}.md")),
            scope: MemoryScope::ProjectPrivate,
            mtime: SystemTime::UNIX_EPOCH,
        };
        let entries = vec![
            (
                mk("prefer-ripgrep", "Use rg for search"),
                "Always reach for ripgrep over grep.".to_string(),
            ),
            (
                mk("editor-choice", "Editor preference"),
                "The user likes neovim.".to_string(),
            ),
            (
                mk("ci-flow", "CI conventions"),
                "Run just check before every PR.".to_string(),
            ),
        ];

        // Body-only match returns the matching line as the snippet.
        let hits = search_entries(entries.clone(), "neovim");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].entry.name, "editor-choice");
        assert!(hits[0].snippet.contains("neovim"));

        // Case-insensitive; matches in the name.
        assert_eq!(search_entries(entries.clone(), "RIPGREP").len(), 1);

        // Description match with no body hit falls back to the description.
        let desc_hits = search_entries(entries.clone(), "conventions");
        assert_eq!(desc_hits.len(), 1);
        assert_eq!(desc_hits[0].snippet, "CI conventions");

        // Blank query and unmatched query both return nothing.
        assert!(search_entries(entries.clone(), "   ").is_empty());
        assert!(search_entries(entries, "nonexistent-xyz").is_empty());
    }

    #[test]
    fn clip_chars_truncates_on_char_boundary() {
        assert_eq!(clip_chars("short", 10), "short");
        let clipped = clip_chars(&"a".repeat(200), 160);
        assert_eq!(clipped.chars().count(), 161); // 160 kept + one ellipsis
        assert!(clipped.ends_with('…'));
    }

    #[test]
    fn write_to_dir_redacts_name_and_tags() {
        // F9: a credential pasted into the name or a tag must be scrubbed too —
        // the name re-enters the always-loaded index, and tags persist in
        // frontmatter. Redacting only description+body would still leak.
        let _lock = FS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("redact_name_tags");
        let path = write_to_dir(
            &dir,
            "leaked key sk-ant-api03-abcdefghijklmnop",
            "desc",
            MemoryScope::Global,
            &["env-OPENAI_API_KEY=sk-abcdefghijklmnop1234".to_string()],
            "body",
        )
        .unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        assert!(
            !raw.contains("sk-ant-api03-abcdefghijklmnop"),
            "name secret leaked: {raw}"
        );
        assert!(
            !raw.contains("sk-abcdefghijklmnop1234"),
            "tag secret leaked: {raw}"
        );
        assert!(
            raw.contains("[REDACTED]"),
            "expected redaction marker: {raw}"
        );
        // The credential must not survive in the on-disk filename either.
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        assert!(
            !stem.contains("abcdefghijklmnop"),
            "secret leaked into filename: {stem}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_root_falls_back_to_stem_and_first_line() {
        let _lock = FS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("fallback");
        fs::write(dir.join("bare-note.md"), "first meaningful line\nmore").unwrap();
        let entries = load_root(&dir, MemoryScope::Global);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "bare-note");
        assert_eq!(entries[0].description, "first meaningful line");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn render_index_groups_by_scope_and_excludes_body() {
        let entries = vec![
            MemoryEntry {
                name: "g".into(),
                description: "global fact".into(),
                path: PathBuf::from("/g/g.md"),
                scope: MemoryScope::Global,
                mtime: UNIX_EPOCH,
            },
            MemoryEntry {
                name: "p".into(),
                description: "private fact".into(),
                path: PathBuf::from("/p/p.md"),
                scope: MemoryScope::ProjectPrivate,
                mtime: UNIX_EPOCH,
            },
        ];
        let (index, truncated) = render_index(&entries, MAX_MEMORY_INDEX_BYTES);
        assert!(!truncated);
        assert!(index.contains("# Memory"));
        assert!(index.contains("## Global"));
        assert!(index.contains("## Project (private)"));
        assert!(index.contains("[g] global fact"));
        assert!(index.contains("[p] private fact"));
        // Global section precedes private (most specific last).
        assert!(index.find("global fact") < index.find("private fact"));
    }

    #[test]
    fn render_index_truncates_when_oversized() {
        let entries: Vec<MemoryEntry> = (0..200)
            .map(|i| MemoryEntry {
                name: format!("name-{i}"),
                description: "a".repeat(80),
                path: PathBuf::from(format!("/m/name-{i}.md")),
                scope: MemoryScope::Global,
                mtime: UNIX_EPOCH,
            })
            .collect();
        let (index, truncated) = render_index(&entries, 1_000);
        assert!(truncated);
        assert!(index.ends_with(MEMORY_INDEX_TRUNCATION_MARKER));
    }

    #[test]
    fn find_git_root_detects_dot_git() {
        let _lock = FS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = temp_dir("gitroot");
        fs::create_dir(root.join(".git")).unwrap();
        let sub = root.join("a/b");
        fs::create_dir_all(&sub).unwrap();
        assert_eq!(find_git_root(&sub).as_deref(), Some(root.as_path()));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn find_git_root_none_without_repo() {
        let _lock = FS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("norepo");
        // No .git anywhere up to a sentinel; walk eventually returns None or a
        // real ancestor repo. Assert it does not falsely claim `dir` is a root.
        assert_ne!(find_git_root(&dir).as_deref(), Some(dir.as_path()));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_in_dir_round_trip() {
        let _lock = FS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("delete");
        let path = write_to_dir(&dir, "doomed", "x", MemoryScope::Global, &[], "bye").unwrap();
        assert!(path.exists());
        // Mirror delete_memory's match-then-remove against the single root.
        let entries = load_root(&dir, MemoryScope::Global);
        assert_eq!(entries.len(), 1);
        fs::remove_file(&entries[0].path).unwrap();
        assert!(load_root(&dir, MemoryScope::Global).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }
}
