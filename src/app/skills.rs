//! SKILL.md discovery + the always-injected skills index.
//!
//! Progressive disclosure without a synthetic tool: at startup we discover
//! `SKILL.md` playbooks (project > user > enabled plugins; within project and
//! user, Mermaid's own directory beats `.claude/`, which beats `.agents/` —
//! see `file_assets::asset_roots`), render a compact
//! index (name, one-line description, absolute path), and inject it into the
//! instructions channel — the same pattern as the memory index. The model
//! activates a skill by reading its `SKILL.md` with the existing policy-gated
//! `read_file`, so activation is honest in the transcript and costs zero
//! per-request tool-schema bytes.
//!
//! Loading is startup-only (no watcher): skills are rarely-edited authored
//! artifacts; restart to pick up changes.

use mermaid_domain::{LoadedSkills, SkillEntry, SkillSource};
use std::path::{Path, PathBuf};

/// Hard cap on indexed skills — the index is prompt real estate.
pub const MAX_SKILLS: usize = 64;
/// Per-entry description clamp (chars) so one verbose skill can't hog the index.
const MAX_DESCRIPTION_CHARS: usize = 200;
/// Byte budget for the rendered index block.
const MAX_INDEX_BYTES: usize = 8 * 1024;
/// Bounded read for each SKILL.md — only the frontmatter matters here, and the
/// model reads the full body itself on activation.
const MAX_SKILL_FILE_BYTES: usize = 8 * 1024;

/// Discover every skill visible from `cwd`, or `None` when there are none.
/// Never errors: an unreadable root or file is skipped (per-file tolerance) —
/// a broken skill must not take down startup.
#[must_use]
pub fn load(cwd: &Path) -> Option<LoadedSkills> {
    let mut groups: Vec<Vec<SkillEntry>> = crate::app::file_assets::asset_roots(cwd)
        .into_iter()
        .map(|root| discover_dir(&root.dir.join("skills"), root.source))
        .collect();
    groups.push(plugin_entries());
    let entries = merge_by_precedence(groups);
    if entries.is_empty() {
        return None;
    }
    let index = render_index(&entries);
    Some(LoadedSkills { entries, index })
}

/// Read `root/<name>/SKILL.md` for every subdirectory of `root`, sorted by
/// skill name for a deterministic index. Missing root ⇒ empty.
fn discover_dir(root: &Path, source: SkillSource) -> Vec<SkillEntry> {
    let mut entries = Vec::new();
    let Ok(read) = std::fs::read_dir(root) else {
        return entries;
    };
    for dir in read.flatten() {
        let path = dir.path();
        if !dir.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        if let Some(entry) = read_skill_entry(&path.join("SKILL.md"), source) {
            entries.push(entry);
        }
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
}

/// Parse one SKILL.md into an entry. `None` when the file is missing or
/// unreadable — discovery is tolerant so one broken skill never hides the rest.
fn read_skill_entry(path: &Path, source: SkillSource) -> Option<SkillEntry> {
    let raw = match mermaid_model::utils::read_file_capped(path, MAX_SKILL_FILE_BYTES) {
        Ok((bytes, _truncated)) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "skills: skipping unreadable SKILL.md");
            return None;
        },
    };
    let (name, description) = parse_skill_frontmatter(&raw);
    // Name falls back to the containing directory (the conventional skill id).
    let name = name.or_else(|| {
        path.parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .map(str::to_string)
    })?;
    let mut description = description.unwrap_or_default();
    if description.chars().count() > MAX_DESCRIPTION_CHARS {
        description = description.chars().take(MAX_DESCRIPTION_CHARS).collect();
        description.push_str("...");
    }
    Some(SkillEntry {
        name,
        description,
        path: path.to_path_buf(),
        source,
    })
}

/// Extract `name:` / `description:` from a leading `---` frontmatter fence,
/// with the first non-empty body line as the description fallback. Simple
/// line-based parsing (Claude Code-compatible frontmatter needs no YAML dep);
/// a missing or unclosed fence means the whole file is body.
fn parse_skill_frontmatter(raw: &str) -> (Option<String>, Option<String>) {
    let (name, description, _) = parse_frontmatter_with_body(raw);
    (name, description)
}

/// [`parse_skill_frontmatter`] plus the body after the fence — the shared
/// dialect for skills AND prompt commands (`app::plugin_assets`,
/// `app::file_assets`).
pub(crate) fn parse_frontmatter_with_body(raw: &str) -> (Option<String>, Option<String>, String) {
    let (fields, body) = parse_frontmatter_fields(raw);
    let field = |key: &str| {
        fields
            .iter()
            .find(|(k, v)| k == key && !v.is_empty())
            .map(|(_, v)| v.clone())
    };
    let first_body_line = body
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string);
    (
        field("name"),
        field("description").or(first_body_line),
        body,
    )
}

/// Split a leading `---` frontmatter fence into `(key, value)` pairs and the
/// body after it. Line-based, no YAML dependency, but it reads the YAML that
/// Claude Code and Codex files use in practice: `key: value` (quotes
/// stripped), a list as `key: [a, b]` or as `- item` lines under the key
/// (joined with `, `), and text continued on indented lines, plain or after
/// `key: |` / `key: >` (joined with spaces). A missing or unclosed fence means the whole
/// file is body and there are no fields.
pub(crate) fn parse_frontmatter_fields(raw: &str) -> (Vec<(String, String)>, String) {
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let mut lines = raw.lines();
    if lines.next().map(str::trim) != Some("---") {
        return (Vec::new(), raw.lines().collect::<Vec<_>>().join("\n"));
    }
    let mut fields: Vec<(String, String)> = Vec::new();
    let mut fm_lines = Vec::new();
    let mut closed = false;
    for line in lines.by_ref() {
        if line.trim() == "---" {
            closed = true;
            break;
        }
        fm_lines.push(line);
    }
    if !closed {
        return (Vec::new(), raw.lines().collect::<Vec<_>>().join("\n"));
    }
    // Whether the last key opened a `|`/`>` block (continuation lines are
    // text) rather than a plain or list value (continuation lines are items).
    let mut in_block = false;
    for line in fm_lines {
        let indented = line.starts_with(' ') || line.starts_with('\t');
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some((_, value)) = fields.last_mut() {
            if in_block && indented {
                if !value.is_empty() {
                    value.push(' ');
                }
                value.push_str(trimmed);
                continue;
            }
            if let Some(item) = trimmed
                .strip_prefix("- ")
                .or((trimmed == "-").then_some(""))
            {
                let item = unquote(item.trim());
                if !item.is_empty() {
                    if !value.is_empty() {
                        value.push_str(", ");
                    }
                    value.push_str(item);
                }
                continue;
            }
            // A plain value continued on indented lines (YAML folds them).
            if indented && !value.is_empty() {
                value.push(' ');
                value.push_str(trimmed);
                continue;
            }
        }
        if indented {
            continue; // nested mapping we don't read
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        let value = value.trim();
        in_block = matches!(value, "|" | ">" | "|-" | ">-" | "|+" | ">+");
        let value = if in_block {
            String::new()
        } else if let Some(list) = value.strip_prefix('[').and_then(|v| v.strip_suffix(']')) {
            list.split(',')
                .map(|item| unquote(item.trim()))
                .filter(|item| !item.is_empty())
                .collect::<Vec<_>>()
                .join(", ")
        } else {
            unquote(value).to_string()
        };
        fields.push((key.trim().to_string(), value));
    }
    (fields, lines.collect::<Vec<_>>().join("\n"))
}

/// Strip one pair of matching outer quotes.
fn unquote(value: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = value
            .strip_prefix(quote)
            .and_then(|v| v.strip_suffix(quote))
        {
            return inner;
        }
    }
    value
}

/// Resolve a plugin's declared skill paths to canonical SKILL.md files,
/// enforcing the same canonicalize + containment check as hooks: a symlink
/// inside the plugin root must not reach files outside it. A declared entry
/// may be the SKILL.md itself or its containing directory.
fn plugin_skill_paths(canonical_root: &Path, declared: &[String]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in declared {
        let Ok(mut resolved) = std::fs::canonicalize(canonical_root.join(entry)) else {
            continue; // missing skill: nothing to index
        };
        if resolved.is_dir() {
            let Ok(inner) = std::fs::canonicalize(resolved.join("SKILL.md")) else {
                continue;
            };
            resolved = inner;
        }
        if !resolved.starts_with(canonical_root) {
            tracing::warn!(entry = %entry, "plugin skill escapes plugin root; skipping");
            continue;
        }
        out.push(resolved);
    }
    out
}

/// Skills declared by enabled plugins. Store/parse failures degrade to empty —
/// skills are additive context, never a startup blocker.
fn plugin_entries() -> Vec<SkillEntry> {
    let Ok(store) = mermaid_runtime::RuntimeStore::open_default() else {
        return Vec::new();
    };
    let Ok(plugins) = store.plugins().list() else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    for plugin in plugins {
        // Same trust boundary as hooks: only explicitly enabled plugins
        // contribute (a disabled plugin's text still steers the model).
        if !plugin.enabled {
            continue;
        }
        let Ok(manifest) =
            serde_json::from_str::<mermaid_runtime::PluginManifest>(&plugin.manifest_json)
        else {
            continue;
        };
        if manifest.skills.is_empty() {
            continue;
        }
        let Ok(root) = std::fs::canonicalize(&plugin.source) else {
            continue;
        };
        for path in plugin_skill_paths(&root, &manifest.skills) {
            if let Some(entry) = read_skill_entry(&path, SkillSource::Plugin) {
                entries.push(entry);
            }
        }
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
}

/// Merge discovery groups (given in precedence order, highest first) into one
/// list, deduplicating by name — the first occurrence wins, so a project skill
/// shadows a same-named user or plugin skill. Pure; unit-testable.
#[must_use]
pub fn merge_by_precedence(groups: Vec<Vec<SkillEntry>>) -> Vec<SkillEntry> {
    let mut seen = std::collections::HashSet::new();
    let mut merged = Vec::new();
    for group in groups {
        for entry in group {
            if seen.insert(entry.name.clone()) {
                merged.push(entry);
            }
        }
    }
    merged
}

/// Render the always-injected `# Skills` block: header with the activation
/// instruction, then one line per skill, capped at [`MAX_SKILLS`] entries and
/// [`MAX_INDEX_BYTES`] bytes with a `(+N more not listed)` overflow line.
/// Entries arrive precedence-ordered, so overflow drops plugin/user tails
/// before any project skill. Pure; unit-testable.
#[must_use]
pub fn render_index(entries: &[SkillEntry]) -> String {
    let mut out = String::from(
        "# Skills\n\nTask-specific playbooks available on this machine. When a skill's \
         description matches the task at hand, read its SKILL.md with `read_file` \
         before proceeding.\n\n",
    );
    // Reserve room for the overflow line so the byte cap can't orphan it.
    const OVERFLOW_RESERVE: usize = 32;
    let mut listed = 0;
    for entry in entries {
        if listed == MAX_SKILLS {
            break;
        }
        let line = format!(
            "- [{}] {} — {} ({})\n",
            entry.name,
            entry.description,
            entry.path.display(),
            entry.source.label()
        );
        if out.len() + line.len() > MAX_INDEX_BYTES.saturating_sub(OVERFLOW_RESERVE) {
            break;
        }
        out.push_str(&line);
        listed += 1;
    }
    let skipped = entries.len() - listed;
    if skipped > 0 {
        out.push_str(&format!("(+{skipped} more not listed)\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Fresh temp dir per test, mirroring the memory.rs convention (no
    /// tempfile crate — deliberate).
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mermaid-skills-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn entry(name: &str, source: SkillSource) -> SkillEntry {
        SkillEntry {
            name: name.to_string(),
            description: format!("{name} description"),
            path: PathBuf::from(format!("/skills/{name}/SKILL.md")),
            source,
        }
    }

    #[test]
    fn frontmatter_parses_name_and_description() {
        let (name, desc) =
            parse_skill_frontmatter("---\nname: deploy\ndescription: \"Ship it\"\n---\n\nBody.\n");
        assert_eq!(name.as_deref(), Some("deploy"));
        assert_eq!(desc.as_deref(), Some("Ship it"));
    }

    #[test]
    fn frontmatter_missing_description_falls_back_to_first_body_line() {
        let (name, desc) =
            parse_skill_frontmatter("---\nname: deploy\n---\n\nFirst body line.\nSecond.\n");
        assert_eq!(name.as_deref(), Some("deploy"));
        assert_eq!(desc.as_deref(), Some("First body line."));
    }

    #[test]
    fn frontmatter_absent_treats_whole_file_as_body() {
        let (name, desc) = parse_skill_frontmatter("Just a body.\n");
        assert_eq!(name, None);
        assert_eq!(desc.as_deref(), Some("Just a body."));
    }

    #[test]
    fn frontmatter_reads_lists_and_folded_text() {
        let (fields, body) = parse_frontmatter_fields(
            "---\nname: 'reviewer'\ndescription: >\n  Reviews code.\n  Use often.\n\
             tools:\n  - Read\n  - \"Grep\"\nmodel: [a, 'b']\nlong: one\n  two\n\
             metadata:\n  type: x\n---\nBody line\n",
        );
        let get = |k: &str| {
            fields
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("name"), Some("reviewer"));
        assert_eq!(get("description"), Some("Reviews code. Use often."));
        assert_eq!(get("tools"), Some("Read, Grep"));
        assert_eq!(get("model"), Some("a, b"));
        assert_eq!(get("long"), Some("one two"));
        assert_eq!(get("metadata"), Some(""));
        assert_eq!(body, "Body line");
    }

    #[test]
    fn frontmatter_unclosed_fence_is_body() {
        let (name, desc) = parse_skill_frontmatter("---\nname: broken\nno closing fence\n");
        assert_eq!(name, None);
        // The whole raw text is body; its first non-empty line is `---`.
        assert_eq!(desc.as_deref(), Some("---"));
    }

    #[test]
    fn merge_dedupes_by_name_project_wins() {
        let merged = merge_by_precedence(vec![
            vec![entry("deploy", SkillSource::Project)],
            vec![
                entry("deploy", SkillSource::User),
                entry("review", SkillSource::User),
            ],
            vec![entry("review", SkillSource::Plugin)],
        ]);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].name, "deploy");
        assert_eq!(merged[0].source, SkillSource::Project);
        assert_eq!(merged[1].name, "review");
        assert_eq!(merged[1].source, SkillSource::User);
    }

    #[test]
    fn render_index_lists_entries_with_source_labels() {
        let index = render_index(&[
            entry("deploy", SkillSource::Project),
            entry("review", SkillSource::User),
        ]);
        assert!(index.starts_with("# Skills\n"));
        assert!(index.contains("read its SKILL.md with `read_file`"));
        assert!(
            index.contains("- [deploy] deploy description — /skills/deploy/SKILL.md (project)")
        );
        assert!(index.contains("- [review] review description — /skills/review/SKILL.md (user)"));
        assert!(!index.contains("more not listed"));
    }

    #[test]
    fn render_index_caps_entry_count_with_overflow_line() {
        let entries: Vec<SkillEntry> = (0..MAX_SKILLS + 5)
            .map(|i| entry(&format!("skill-{i:03}"), SkillSource::User))
            .collect();
        let index = render_index(&entries);
        assert!(index.contains("skill-000"));
        assert!(index.contains(&format!("skill-{:03}", MAX_SKILLS - 1)));
        assert!(!index.contains(&format!("skill-{MAX_SKILLS:03}")));
        assert!(index.contains("(+5 more not listed)"));
    }

    #[test]
    fn render_index_caps_bytes_with_overflow_line() {
        let entries: Vec<SkillEntry> = (0..MAX_SKILLS)
            .map(|i| SkillEntry {
                name: format!("skill-{i:03}"),
                description: "d".repeat(MAX_DESCRIPTION_CHARS),
                path: PathBuf::from(format!("/skills/skill-{i:03}/SKILL.md")),
                source: SkillSource::User,
            })
            .collect();
        let index = render_index(&entries);
        assert!(index.len() <= MAX_INDEX_BYTES);
        assert!(index.contains("more not listed"));
    }

    #[test]
    fn discover_reads_skill_dirs_sorted_and_tolerates_junk() {
        let dir = temp_dir("discover");
        let root = dir.join("skills");
        fs::create_dir_all(root.join("zeta")).unwrap();
        fs::write(
            root.join("zeta").join("SKILL.md"),
            "---\nname: zeta\ndescription: Last alphabetically\n---\nBody",
        )
        .unwrap();
        fs::create_dir_all(root.join("alpha")).unwrap();
        fs::write(root.join("alpha").join("SKILL.md"), "No frontmatter body.").unwrap();
        // A dir without SKILL.md and a stray file are both skipped.
        fs::create_dir_all(root.join("empty")).unwrap();
        fs::write(root.join("README.md"), "not a skill").unwrap();
        let entries = discover_dir(&root, SkillSource::Project);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "alpha"); // dir-name fallback
        assert_eq!(entries[0].description, "No frontmatter body.");
        assert_eq!(entries[1].name, "zeta");
        assert!(entries[1].path.ends_with("zeta/SKILL.md"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn discover_missing_root_is_empty() {
        let dir = temp_dir("missing-root");
        assert!(discover_dir(&dir.join("nope"), SkillSource::User).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_discovers_project_skills_from_git_root() {
        let dir = temp_dir("load-project");
        fs::create_dir(dir.join(".git")).unwrap();
        let skill_dir = dir.join(".mermaid").join("skills").join("demo");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: demo\ndescription: Demo skill\n---\nSteps.",
        )
        .unwrap();
        // Load from a SUBDIRECTORY so the git-root walk is exercised.
        let sub = dir.join("src");
        fs::create_dir_all(&sub).unwrap();
        let loaded = load(&sub).expect("project skill should be discovered");
        assert!(loaded.entries.iter().any(|e| e.name == "demo"));
        assert!(loaded.index.contains("[demo] Demo skill"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn plugin_skill_paths_enforce_containment() {
        let dir = temp_dir("plugin-containment");
        let root = dir.join("plugin");
        let outside = dir.join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("SKILL.md"), "escaped").unwrap();
        // In-root skill as a direct file.
        let inside = root.join("skills").join("ok");
        fs::create_dir_all(&inside).unwrap();
        fs::write(inside.join("SKILL.md"), "---\nname: ok\n---\nBody").unwrap();
        // Symlink pointing outside the root must be rejected.
        std::os::unix::fs::symlink(&outside, root.join("evil")).unwrap();
        let canonical_root = fs::canonicalize(&root).unwrap();
        let paths = plugin_skill_paths(
            &canonical_root,
            &["skills/ok".to_string(), "evil".to_string()],
        );
        assert_eq!(paths.len(), 1);
        assert!(paths[0].ends_with("ok/SKILL.md"));
        let _ = fs::remove_dir_all(&dir);
    }
}
