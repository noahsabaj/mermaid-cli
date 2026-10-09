//! Prompt commands and agent types from plain files — no plugin needed — and
//! the shared list of directories Mermaid reads them (and skills) from.
//!
//! Mermaid reads its own directories and the ones Claude Code and the
//! `.agents/` convention use, so a project or user moving from another tool
//! keeps its skills, commands and agents:
//!
//! | scope   | directories, highest precedence first                   |
//! |---------|---------------------------------------------------------|
//! | project | `<git-root>/.mermaid/`, `<git-root>/.claude/`, `<git-root>/.agents/` |
//! | user    | `<config-dir>/` (`~/.config/mermaid/`), `~/.claude/`, `~/.agents/` |
//!
//! Under each: `skills/<name>/SKILL.md` (see `app::skills`),
//! `commands/**/*.md` (prompt commands) and `agents/*.md` (agent types).
//! Project beats user, and a same-named entry from a higher directory hides
//! the lower one, so `.mermaid/` wins over `.claude/`.
//!
//! Like plugin assets, these load once at startup; restart to pick up edits.

use std::path::{Path, PathBuf};

use mermaid_domain::{AgentTypeConfig, PluginCommand, SkillSource};

/// One directory that may hold `skills/`, `commands/` and `agents/`.
#[derive(Debug, Clone)]
pub struct AssetRoot {
    pub dir: PathBuf,
    pub source: SkillSource,
}

/// Every asset directory visible from `cwd`, highest precedence first.
/// Directories that don't exist are listed anyway; readers skip them.
#[must_use]
pub fn asset_roots(cwd: &Path) -> Vec<AssetRoot> {
    let project = crate::app::memory::find_git_root(cwd).unwrap_or_else(|| cwd.to_path_buf());
    let mut roots: Vec<AssetRoot> = [".mermaid", ".claude", ".agents"]
        .iter()
        .map(|name| AssetRoot {
            dir: project.join(name),
            source: SkillSource::Project,
        })
        .collect();
    if let Ok(dir) = crate::app::get_config_dir() {
        roots.push(AssetRoot {
            dir,
            source: SkillSource::User,
        });
    }
    if let Some(dirs) = directories::BaseDirs::new() {
        for name in [".claude", ".agents"] {
            roots.push(AssetRoot {
                dir: dirs.home_dir().join(name),
                source: SkillSource::User,
            });
        }
    }
    // A project at `$HOME` would list `~/.claude` twice; keep the first.
    let mut seen = std::collections::HashSet::new();
    roots.retain(|root| seen.insert(root.dir.clone()));
    roots
}

/// Prompt commands and agent types found in the asset directories.
#[derive(Debug, Default)]
pub struct FileAssets {
    /// Deduplicated by name, highest precedence first.
    pub commands: Vec<PluginCommand>,
    /// Deduplicated by name, highest precedence first.
    pub agent_types: Vec<(String, AgentTypeConfig)>,
    pub warnings: Vec<String>,
}

/// How deep `commands/` is walked. Claude Code namespaces commands by
/// subdirectory (`commands/frontend/component.md` is `/component`).
const MAX_COMMAND_DEPTH: usize = 3;
/// Bounded read per file; a command or agent prompt is a page, not a book.
const MAX_ASSET_FILE_BYTES: usize = 64 * 1024;
/// Per-type description clamp, matching the skills index.
const MAX_DESCRIPTION_CHARS: usize = 200;

/// Read every asset directory visible from `cwd`.
#[must_use]
pub fn load(cwd: &Path) -> FileAssets {
    load_from_roots(&asset_roots(cwd))
}

/// Load file assets and then enabled plugins' assets, fold both into the
/// merged `config` (agent types, plugin MCP servers), and return the prompt
/// commands plus the warnings to show at startup. Config entries win over
/// files, and files win over plugins.
pub fn load_with_plugins(
    config: &mut mermaid_domain::Config,
    cwd: &Path,
) -> (Vec<PluginCommand>, Vec<String>) {
    let files = load(cwd);
    let mut warnings = apply(config, &files);
    let plugins = crate::app::plugin_assets::load();
    warnings.extend(crate::app::plugin_assets::apply(config, &plugins));
    let mut commands = files.commands;
    for command in plugins.commands {
        if commands.iter().any(|c| c.name == command.name) {
            warnings.push(format!(
                "plugin command '/{}' is shadowed by a command file; using the file",
                command.name
            ));
            continue;
        }
        commands.push(command);
    }
    (commands, warnings)
}

/// Pure over the given roots, so tests need no real home directory.
#[must_use]
pub fn load_from_roots(roots: &[AssetRoot]) -> FileAssets {
    let mut assets = FileAssets::default();
    for root in roots {
        let mut files = Vec::new();
        collect_markdown(&root.dir.join("commands"), MAX_COMMAND_DEPTH, &mut files);
        for path in files {
            let Some(raw) = read_asset(&path) else {
                continue;
            };
            match parse_command(&path, &raw, root.source) {
                Ok(command) => {
                    if !assets.commands.iter().any(|c| c.name == command.name) {
                        assets.commands.push(command);
                    }
                },
                Err(warning) => assets.warnings.push(warning),
            }
        }
        let mut files = Vec::new();
        collect_markdown(&root.dir.join("agents"), 1, &mut files);
        for path in files {
            let Some(raw) = read_asset(&path) else {
                continue;
            };
            match parse_agent(&path, &raw, root.source) {
                Ok((name, agent)) => {
                    if !assets.agent_types.iter().any(|(n, _)| *n == name) {
                        assets.agent_types.push((name, agent));
                    }
                },
                Err(warning) => assets.warnings.push(warning),
            }
        }
    }
    assets
}

/// Fold file agent types into the merged config for names it doesn't define
/// already. Returns the warnings to surface at startup.
pub fn apply(config: &mut mermaid_domain::Config, assets: &FileAssets) -> Vec<String> {
    let mut warnings = assets.warnings.clone();
    for (name, agent) in &assets.agent_types {
        if config.agents.types.contains_key(name) {
            warnings.push(format!(
                "agent file '{name}' is shadowed by [agents.types.{name}] in config; \
                 using the config entry"
            ));
            continue;
        }
        config.agents.types.insert(name.clone(), agent.clone());
    }
    warnings
}

/// Append every `*.md` file under `dir` (to `depth` levels) to `out`, sorted
/// per directory so the first of two same-named files is deterministic.
/// A missing or unreadable directory adds nothing.
fn collect_markdown(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth == 0 {
        return;
    }
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<PathBuf> = read.flatten().map(|e| e.path()).collect();
    entries.sort();
    let (dirs, files): (Vec<PathBuf>, Vec<PathBuf>) = entries.into_iter().partition(|p| p.is_dir());
    out.extend(
        files
            .into_iter()
            .filter(|p| p.extension().is_some_and(|ext| ext == "md")),
    );
    for sub in dirs {
        collect_markdown(&sub, depth - 1, out);
    }
}

fn read_asset(path: &Path) -> Option<String> {
    match mermaid_model::utils::read_file_capped(path, MAX_ASSET_FILE_BYTES) {
        Ok((bytes, _truncated)) => Some(String::from_utf8_lossy(&bytes).into_owned()),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "skipping unreadable asset file");
            None
        },
    }
}

fn file_stem(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// One `commands/*.md` file as a prompt command. The name is the file stem
/// (Claude Code's rule) unless the frontmatter sets `name:`.
fn parse_command(path: &Path, raw: &str, source: SkillSource) -> Result<PluginCommand, String> {
    let (fields, body) = super::skills::parse_frontmatter_fields(raw);
    let field = |key: &str| {
        fields
            .iter()
            .find(|(k, v)| k == key && !v.is_empty())
            .map(|(_, v)| v.clone())
    };
    let name = field("name").unwrap_or_else(|| file_stem(path));
    let shown = path.display();
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(format!(
            "command file {shown}: name '{name}' is not [a-z0-9-]+; skipped"
        ));
    }
    if body.trim().is_empty() {
        return Err(format!("command file {shown}: empty body; skipped"));
    }
    if mermaid_domain::slash_commands::COMMAND_REGISTRY
        .iter()
        .any(|c| c.name == name || c.aliases.contains(&name.as_str()))
    {
        return Err(format!(
            "command file {shown}: '/{name}' shadows a built-in command; skipped"
        ));
    }
    Ok(PluginCommand {
        name,
        description: field("description").unwrap_or_default(),
        body: body.trim().to_string(),
        origin: source.label().to_string(),
    })
}

/// Tool names from Claude Code agent files that have no Mermaid counterpart
/// and need none (subagents can't spawn agents; to-do and plan tools are the
/// parent's). Dropped without a warning.
const IGNORED_TOOLS: &[&str] = &[
    "Task",
    "Agent",
    "TodoWrite",
    "TodoRead",
    "ExitPlanMode",
    "EnterPlanMode",
    "BashOutput",
    "KillShell",
    "KillBash",
    "SlashCommand",
    "Skill",
    "AskUserQuestion",
    "ListMcpResourcesTool",
    "ReadMcpResourceTool",
];

/// Write-class child tools. An agent whose tools include none of them is
/// read-only, and gets a `read_only` safety ceiling unless it sets one.
const WRITE_TOOLS: &[&str] = &[
    "write_file",
    "edit_file",
    "apply_patch",
    "delete_file",
    "create_directory",
    "execute_command",
];

/// Map one tool name from an agent file to Mermaid child tools. Accepts
/// Mermaid's own names and Claude Code's (`Read`, `Bash(git:*)`,
/// `mcp__server__tool`, ...). `None` for a name with no counterpart.
/// The flag is true when the tool only searches, so a shell it brings in is
/// for reading.
fn map_tool(name: &str) -> Option<(&'static [&'static str], bool)> {
    let base = name.split('(').next().unwrap_or(name).trim();
    if base.starts_with("mcp__") {
        return Some((&["mcp"], false));
    }
    if let Some(own) = crate::providers::tool::subagent::CHILD_TOOL_NAMES
        .iter()
        .find(|t| **t == base)
    {
        return Some((std::slice::from_ref(own), false));
    }
    Some(match base {
        "Read" | "NotebookRead" => (&["read_file"], true),
        // Mermaid searches with the shell (rg, find, ls), as the built-in
        // `explore` type does.
        "Grep" | "Glob" | "LS" => (&["read_file", "execute_command"], true),
        "Edit" | "MultiEdit" | "NotebookEdit" => {
            (&["edit_file", "apply_patch", "write_file"], false)
        },
        "Write" => (&["write_file", "create_directory"], false),
        "Bash" => (&["execute_command"], false),
        "WebFetch" => (&["web_fetch"], false),
        "WebSearch" => (&["web_search"], false),
        _ => return None,
    })
}

/// One `agents/*.md` file as an agent type: Claude Code's subagent format
/// (`name`, `description`, `tools`, `model`; the body is the system prompt)
/// plus Mermaid's `safety` and `isolation`.
fn parse_agent(
    path: &Path,
    raw: &str,
    source: SkillSource,
) -> Result<(String, AgentTypeConfig), String> {
    let (fields, body) = super::skills::parse_frontmatter_fields(raw);
    let field = |key: &str| {
        fields
            .iter()
            .find(|(k, v)| k == key && !v.is_empty())
            .map(|(_, v)| v.clone())
    };
    let name = field("name").unwrap_or_else(|| file_stem(path));
    let shown = path.display();
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!(
            "agent file {shown}: name '{name}' is not [A-Za-z0-9_-]+; skipped"
        ));
    }
    // A project file must not retune a built-in: `explore` is read-only by
    // construction, and a cloned repository must not loosen it. Your own
    // user-level files may, as `[agents.types.explore]` in config may.
    if source == SkillSource::Project && matches!(name.as_str(), "general" | "explore") {
        return Err(format!(
            "agent file {shown}: a project file cannot redefine the built-in '{name}' type; skipped"
        ));
    }

    let mut unknown = Vec::new();
    let tools = field("tools").map(|list| {
        let mut mapped: Vec<String> = Vec::new();
        let mut writes = false;
        let items: Vec<&str> = if list.contains(',') {
            list.split(',').collect()
        } else {
            list.split_whitespace().collect()
        };
        for item in items.into_iter().map(str::trim).filter(|i| !i.is_empty()) {
            match map_tool(item) {
                Some((names, search)) => {
                    for tool in names {
                        if WRITE_TOOLS.contains(tool) && !search {
                            writes = true;
                        }
                        if !mapped.iter().any(|m| m == tool) {
                            mapped.push((*tool).to_string());
                        }
                    }
                },
                None if IGNORED_TOOLS.contains(&item.split('(').next().unwrap_or(item)) => {},
                None => unknown.push(item.to_string()),
            }
        }
        (mapped, writes)
    });

    let safety = match field("safety") {
        Some(s) => {
            if mermaid_runtime::SafetyMode::parse(&s).is_none() {
                return Err(format!(
                    "agent file {shown}: safety '{s}' is not one of read_only/ask/auto/full_access; skipped"
                ));
            }
            Some(s)
        },
        // Listed tools that can't write: hold the child to read-only, so a
        // shell brought in for searching can't be used to change files.
        None => match &tools {
            Some((_, false)) => Some("read_only".to_string()),
            _ => None,
        },
    };
    // Claude Code's `model` is an alias (`sonnet`, `inherit`) or a vendor
    // id; only Mermaid's `provider/model` form means anything here.
    let model = field("model").filter(|m| m.contains('/'));
    let description = field("description").map(|d| {
        if d.chars().count() > MAX_DESCRIPTION_CHARS {
            let mut clipped: String = d.chars().take(MAX_DESCRIPTION_CHARS).collect();
            clipped.push_str("...");
            clipped
        } else {
            d
        }
    });
    let preamble = Some(body.trim().to_string()).filter(|b| !b.is_empty());
    if !unknown.is_empty() {
        tracing::info!(
            agent = %name,
            tools = %unknown.join(", "),
            "agent file lists tools with no Mermaid counterpart; ignored"
        );
    }
    Ok((
        name,
        AgentTypeConfig {
            tools: tools.map(|(mapped, _)| mapped),
            safety,
            preamble,
            model,
            isolation: field("isolation"),
            description,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mermaid-file-assets-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, body: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    fn root(dir: PathBuf, source: SkillSource) -> AssetRoot {
        AssetRoot { dir, source }
    }

    #[test]
    fn asset_roots_list_mermaid_then_claude_then_agents() {
        let dir = temp_dir("roots");
        fs::create_dir_all(dir.join(".git")).unwrap();
        let roots = asset_roots(&dir);
        assert_eq!(roots[0].dir, dir.join(".mermaid"));
        assert_eq!(roots[1].dir, dir.join(".claude"));
        assert_eq!(roots[2].dir, dir.join(".agents"));
        assert!(roots[..3].iter().all(|r| r.source == SkillSource::Project));
        assert!(roots[3..].iter().all(|r| r.source == SkillSource::User));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn commands_load_from_claude_dirs_and_mermaid_wins() {
        let dir = temp_dir("commands");
        let mermaid = dir.join(".mermaid");
        let claude = dir.join(".claude");
        write(
            &claude.join("commands/fix-issue.md"),
            "---\ndescription: Fix a GitHub issue\nargument-hint: [number]\n---\nFix issue #$1.\n",
        );
        write(
            &claude.join("commands/frontend/component.md"),
            "Build a $ARGUMENTS component.\n",
        );
        write(&claude.join("commands/deploy.md"), "Claude deploy\n");
        write(&mermaid.join("commands/deploy.md"), "Mermaid deploy\n");
        write(&claude.join("commands/notes.txt"), "not a command\n");
        let assets = load_from_roots(&[
            root(mermaid, SkillSource::Project),
            root(claude, SkillSource::Project),
        ]);
        assert!(assets.warnings.is_empty(), "{:?}", assets.warnings);
        let names: Vec<&str> = assets.commands.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["deploy", "fix-issue", "component"]);
        assert_eq!(assets.commands[0].body, "Mermaid deploy");
        assert_eq!(assets.commands[1].description, "Fix a GitHub issue");
        assert_eq!(assets.commands[1].origin, "project");
        assert_eq!(assets.commands[1].expand("42"), "Fix issue #42.");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn command_files_cannot_shadow_builtins_or_use_bad_names() {
        let dir = temp_dir("command-names");
        write(&dir.join("commands/help.md"), "hijack\n");
        write(&dir.join("commands/Bad_Name.md"), "body\n");
        write(
            &dir.join("commands/empty.md"),
            "---\ndescription: nothing\n---\n\n",
        );
        let assets = load_from_roots(&[root(dir.clone(), SkillSource::User)]);
        assert!(assets.commands.is_empty());
        assert_eq!(assets.warnings.len(), 3, "{:?}", assets.warnings);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn claude_agent_file_maps_to_an_agent_type() {
        let dir = temp_dir("agents");
        write(
            &dir.join("agents/code-reviewer.md"),
            "---\nname: code-reviewer\ndescription: Reviews code for quality.\n  Use after edits.\n\
             tools: Read, Grep, Glob, TodoWrite\nmodel: sonnet\n---\n\nYou are a senior reviewer.\n",
        );
        write(
            &dir.join("agents/fixer.md"),
            "---\ndescription: >\n  Fixes failing tests.\ntools:\n  - Read\n  - Edit\n  - Bash(cargo:*)\n\
             model: ollama/qwen3:8b\n---\nFix it.\n",
        );
        write(
            &dir.join("agents/all-tools.md"),
            "---\ndescription: Anything\n---\nDo it.\n",
        );
        let assets = load_from_roots(&[root(dir.clone(), SkillSource::Project)]);
        assert!(assets.warnings.is_empty(), "{:?}", assets.warnings);
        let get = |name: &str| {
            assets
                .agent_types
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, t)| t.clone())
                .unwrap()
        };

        let reviewer = get("code-reviewer");
        assert_eq!(
            reviewer.tools.as_deref(),
            Some(&["read_file".to_string(), "execute_command".to_string()][..])
        );
        // No write tool listed: held to read-only.
        assert_eq!(reviewer.safety.as_deref(), Some("read_only"));
        // A Claude model alias means nothing to Mermaid.
        assert_eq!(reviewer.model, None);
        assert_eq!(
            reviewer.description.as_deref(),
            Some("Reviews code for quality. Use after edits.")
        );
        assert_eq!(
            reviewer.preamble.as_deref(),
            Some("You are a senior reviewer.")
        );

        let fixer = get("fixer");
        let tools = fixer.tools.unwrap();
        for t in [
            "read_file",
            "edit_file",
            "apply_patch",
            "write_file",
            "execute_command",
        ] {
            assert!(tools.iter().any(|x| x == t), "{t} missing from {tools:?}");
        }
        assert_eq!(fixer.safety, None);
        assert_eq!(fixer.model.as_deref(), Some("ollama/qwen3:8b"));
        assert_eq!(fixer.description.as_deref(), Some("Fixes failing tests."));

        let all = get("all-tools");
        assert_eq!(all.tools, None);
        assert_eq!(all.safety, None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn project_agent_files_cannot_redefine_builtins_but_user_files_can() {
        let dir = temp_dir("agent-builtins");
        write(
            &dir.join("agents/explore.md"),
            "---\ndescription: loose\n---\nDo anything.\n",
        );
        let project = load_from_roots(&[root(dir.clone(), SkillSource::Project)]);
        assert!(project.agent_types.is_empty());
        assert!(
            project
                .warnings
                .iter()
                .any(|w| w.contains("built-in 'explore'")),
            "{:?}",
            project.warnings
        );
        let user = load_from_roots(&[root(dir.clone(), SkillSource::User)]);
        assert_eq!(user.agent_types.len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn agent_file_with_bad_safety_is_skipped() {
        let dir = temp_dir("agent-safety");
        write(&dir.join("agents/x.md"), "---\nsafety: yolo\n---\nbody\n");
        let assets = load_from_roots(&[root(dir.clone(), SkillSource::User)]);
        assert!(assets.agent_types.is_empty());
        assert!(assets.warnings.iter().any(|w| w.contains("yolo")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_keeps_config_types_and_adds_the_rest() {
        let mut config = mermaid_domain::Config::default();
        config.agents.types.insert(
            "scout".to_string(),
            AgentTypeConfig {
                preamble: Some("config".to_string()),
                ..AgentTypeConfig::default()
            },
        );
        let assets = FileAssets {
            agent_types: vec![
                (
                    "scout".to_string(),
                    AgentTypeConfig {
                        preamble: Some("file".to_string()),
                        ..AgentTypeConfig::default()
                    },
                ),
                ("reviewer".to_string(), AgentTypeConfig::default()),
            ],
            ..FileAssets::default()
        };
        let warnings = apply(&mut config, &assets);
        assert_eq!(
            config.agents.types["scout"].preamble.as_deref(),
            Some("config")
        );
        assert!(config.agents.types.contains_key("reviewer"));
        assert!(
            warnings.iter().any(|w| w.contains("shadowed")),
            "{warnings:?}"
        );
    }
}
