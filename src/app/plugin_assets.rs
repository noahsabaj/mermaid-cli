//! Plugin-contributed assets beyond skills: MCP servers (`manifest.mcp`),
//! slash-command prompts (`manifest.prompts`), and agent types
//! (`manifest.agents`).
//!
//! One loader, one store read: `load()` walks the installed-plugin store
//! once (enabled plugins only — the same trust boundary as hooks: enabling a
//! plugin that declares MCP servers grants command execution, an explicit
//! `mermaid plugin enable` decision) and `apply()` folds the result into the
//! ALREADY-MERGED `Config` at the top of both entrypoints. Deliberately NOT
//! a config layer: the project layer forbids `mcp_servers`/`agents`, and
//! plugin trust is a different boundary than file trust. Everything
//! downstream rides free — `State::new` seeds plugin servers, MCP init
//! starts them (and PR-1 deferral bounds their tools), subagents see the
//! merged agent types, and the recording header captures the merged config
//! (replay-faithful). Like skills, changes require a restart.

use std::collections::HashMap;
use std::path::Path;

use crate::app::{AgentTypeConfig, McpServerConfig};

/// Everything enabled plugins contribute (besides skills), plus the
/// warnings the startup path should surface.
#[derive(Debug, Default)]
pub struct PluginAssets {
    /// MCP servers keyed by their manifest-file name (NOT prefixed — a
    /// prefix would pollute the `mcp__<server>__<tool>` tool names).
    pub mcp_servers: HashMap<String, McpServerConfig>,
    /// Prompt-backed slash commands.
    pub commands: Vec<crate::domain::PluginCommand>,
    /// Agent types, merged into `config.agents.types` for absent names only.
    pub agent_types: HashMap<String, AgentTypeConfig>,
    pub warnings: Vec<String>,
}

/// A plugin's `manifest.mcp` / `manifest.agents` TOML file shapes.
#[derive(serde::Deserialize)]
struct McpBundle {
    #[serde(default)]
    servers: HashMap<String, McpServerConfig>,
}

#[derive(serde::Deserialize)]
struct AgentBundle {
    #[serde(default)]
    types: HashMap<String, AgentTypeConfig>,
}

/// Load every enabled plugin's assets from the runtime store. Degrades to
/// empty on store/parse failure (the skills precedent — a broken plugin
/// must not kill startup). Plugins are visited in sorted-name order so
/// plugin-vs-plugin collisions resolve deterministically.
pub fn load() -> PluginAssets {
    let mut assets = PluginAssets::default();
    let Ok(store) = crate::runtime::RuntimeStore::open_default() else {
        return assets;
    };
    let Ok(mut plugins) = store.plugins().list() else {
        return assets;
    };
    plugins.sort_by(|a, b| a.name.cmp(&b.name));
    for plugin in plugins {
        if !plugin.enabled {
            continue;
        }
        let Ok(manifest) =
            serde_json::from_str::<crate::runtime::PluginManifest>(&plugin.manifest_json)
        else {
            assets.warnings.push(format!(
                "plugin '{}': unreadable manifest; skipped",
                plugin.name
            ));
            continue;
        };
        let Ok(root) = std::fs::canonicalize(&plugin.source) else {
            continue;
        };
        merge_assets(&mut assets, assets_from_manifest(&root, &manifest));
    }
    assets
}

/// Fold `next` into `acc` with first-wins collision policy (plugins arrive
/// in sorted order, so the winner is deterministic).
fn merge_assets(acc: &mut PluginAssets, next: PluginAssets) {
    for (name, server) in next.mcp_servers {
        if acc.mcp_servers.contains_key(&name) {
            acc.warnings.push(format!(
                "plugin MCP server '{name}' is defined by more than one plugin; keeping the first"
            ));
            continue;
        }
        acc.mcp_servers.insert(name, server);
    }
    for command in next.commands {
        if acc.commands.iter().any(|c| c.name == command.name) {
            acc.warnings.push(format!(
                "plugin command '/{}' is defined by more than one plugin; keeping the first",
                command.name
            ));
            continue;
        }
        acc.commands.push(command);
    }
    for (name, agent) in next.agent_types {
        if acc.agent_types.contains_key(&name) {
            acc.warnings.push(format!(
                "plugin agent type '{name}' is defined by more than one plugin; keeping the first"
            ));
            continue;
        }
        acc.agent_types.insert(name, agent);
    }
    acc.warnings.extend(next.warnings);
}

/// Parse one plugin's declared asset files against its canonical root.
/// Pure-ish (filesystem reads only) so tests need no `RuntimeStore`.
pub(crate) fn assets_from_manifest(
    root: &Path,
    manifest: &crate::runtime::PluginManifest,
) -> PluginAssets {
    let mut assets = PluginAssets::default();
    let plugin = &manifest.name;
    for entry in &manifest.mcp {
        let Some(raw) = read_contained(root, entry, plugin, &mut assets.warnings) else {
            continue;
        };
        match toml::from_str::<McpBundle>(&raw) {
            Ok(bundle) => {
                for (name, mut server) in bundle.servers {
                    // A `./`-relative command resolves against the plugin
                    // root (with containment); anything else is PATH-looked-up
                    // like a config-defined server. A url-only entry has an
                    // empty command and loads un-rewritten (validated later
                    // by `transport_kind` at server start).
                    if let Some(rel) = server
                        .command
                        .strip_prefix("./")
                        .map(str::to_string)
                        .filter(|r| !r.is_empty())
                    {
                        match std::fs::canonicalize(root.join(&rel)) {
                            Ok(resolved) if resolved.starts_with(root) => {
                                server.command = resolved.display().to_string();
                            },
                            _ => {
                                assets.warnings.push(format!(
                                    "plugin '{plugin}': MCP server '{name}' command '{}' escapes \
                                     the plugin directory or is missing; skipped",
                                    server.command
                                ));
                                continue;
                            },
                        }
                    }
                    assets.mcp_servers.insert(name, server);
                }
            },
            Err(err) => assets.warnings.push(format!(
                "plugin '{plugin}': MCP bundle {entry} did not parse: {err}"
            )),
        }
    }
    for entry in &manifest.prompts {
        let Some(raw) = read_contained(root, entry, plugin, &mut assets.warnings) else {
            continue;
        };
        let (name, description, body) = super::skills::parse_frontmatter_with_body(&raw);
        let stem = Path::new(entry)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let name = name.unwrap_or(stem);
        if !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            || name.is_empty()
        {
            assets.warnings.push(format!(
                "plugin '{plugin}': prompt name '{name}' is not [a-z0-9-]+; skipped"
            ));
            continue;
        }
        if body.trim().is_empty() {
            assets.warnings.push(format!(
                "plugin '{plugin}': prompt '/{name}' has an empty body; skipped"
            ));
            continue;
        }
        // Builtins always win — a plugin must not shadow /help or /quit.
        if crate::domain::slash_commands::COMMAND_REGISTRY
            .iter()
            .any(|c| c.name == name || c.aliases.contains(&name.as_str()))
        {
            assets.warnings.push(format!(
                "plugin '{plugin}': prompt '/{name}' shadows a built-in command; skipped"
            ));
            continue;
        }
        assets.commands.push(crate::domain::PluginCommand {
            name,
            description: description.unwrap_or_default(),
            body: body.trim().to_string(),
            plugin: plugin.clone(),
        });
    }
    for entry in &manifest.agents {
        let Some(raw) = read_contained(root, entry, plugin, &mut assets.warnings) else {
            continue;
        };
        match toml::from_str::<AgentBundle>(&raw) {
            Ok(bundle) => assets.agent_types.extend(bundle.types),
            Err(err) => assets.warnings.push(format!(
                "plugin '{plugin}': agent bundle {entry} did not parse: {err}"
            )),
        }
    }
    assets
}

/// Read a declared asset file with the same canonicalize + containment check
/// as plugin skills/hooks: a symlink inside the plugin root must not reach
/// files outside it.
fn read_contained(
    root: &Path,
    entry: &str,
    plugin: &str,
    warnings: &mut Vec<String>,
) -> Option<String> {
    let resolved = std::fs::canonicalize(root.join(entry)).ok()?;
    if !resolved.starts_with(root) {
        warnings.push(format!(
            "plugin '{plugin}': asset {entry} escapes the plugin directory; skipped"
        ));
        return None;
    }
    std::fs::read_to_string(&resolved).ok()
}

/// Fold plugin assets into the already-merged `Config`. Config-defined
/// entries always win (a user's `[mcp_servers.x]` / `[agents.types.x]`
/// beats a plugin's); returns the warnings to surface at startup.
pub fn apply(config: &mut crate::app::Config, assets: &PluginAssets) -> Vec<String> {
    let mut warnings = assets.warnings.clone();
    for (name, server) in &assets.mcp_servers {
        if config.mcp_servers.contains_key(name) {
            warnings.push(format!(
                "plugin MCP server '{name}' is shadowed by [mcp_servers.{name}] in config; \
                 using the config entry"
            ));
            continue;
        }
        config.mcp_servers.insert(name.clone(), server.clone());
    }
    for (name, agent) in &assets.agent_types {
        if config.agents.types.contains_key(name) {
            warnings.push(format!(
                "plugin agent type '{name}' is shadowed by [agents.types.{name}] in config; \
                 using the config entry"
            ));
            continue;
        }
        config.agents.types.insert(name.clone(), agent.clone());
    }
    warnings
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_root(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mermaid-plugin-assets-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(&dir).unwrap()
    }

    fn manifest(
        root: &std::path::Path,
        mcp: &[&str],
        prompts: &[&str],
        agents: &[&str],
    ) -> crate::runtime::PluginManifest {
        let _ = root;
        crate::runtime::PluginManifest {
            name: "demo".to_string(),
            version: None,
            description: None,
            skills: vec![],
            agents: agents.iter().map(|s| s.to_string()).collect(),
            hooks: vec![],
            mcp: mcp.iter().map(|s| s.to_string()).collect(),
            capabilities: vec![],
            prompts: prompts.iter().map(|s| s.to_string()).collect(),
            bin: vec![],
        }
    }

    #[test]
    fn fixture_plugin_parses_all_three_asset_kinds() {
        let root = fixture_root("full");
        std::fs::write(
            root.join("servers.toml"),
            "[servers.context7]\ncommand = \"npx\"\nargs = [\"-y\", \"context7\"]\n",
        )
        .unwrap();
        std::fs::write(
            root.join("deploy.md"),
            "---\nname: deploy\ndescription: Deploy the app\n---\nDeploy to $ARGUMENTS now.\n",
        )
        .unwrap();
        std::fs::write(
            root.join("types.toml"),
            "[types.scout]\nsafety = \"read_only\"\npreamble = \"\"\"\nBe brief.\n\"\"\"\n",
        )
        .unwrap();
        let m = manifest(&root, &["servers.toml"], &["deploy.md"], &["types.toml"]);
        let assets = assets_from_manifest(&root, &m);
        assert!(assets.warnings.is_empty(), "{:?}", assets.warnings);
        assert_eq!(assets.mcp_servers["context7"].command, "npx");
        assert_eq!(assets.commands.len(), 1);
        assert_eq!(assets.commands[0].name, "deploy");
        assert_eq!(assets.commands[0].body, "Deploy to $ARGUMENTS now.");
        assert_eq!(
            assets.agent_types["scout"].safety.as_deref(),
            Some("read_only")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn prompt_name_falls_back_to_stem_and_rejects_bad_names() {
        let root = fixture_root("names");
        std::fs::write(root.join("ship-it.md"), "Ship the thing.\n").unwrap();
        std::fs::write(root.join("Bad Name.md"), "body\n").unwrap();
        std::fs::write(root.join("empty.md"), "---\nname: empty\n---\n\n").unwrap();
        let m = manifest(&root, &[], &["ship-it.md", "Bad Name.md", "empty.md"], &[]);
        let assets = assets_from_manifest(&root, &m);
        assert_eq!(assets.commands.len(), 1, "{:?}", assets.warnings);
        assert_eq!(assets.commands[0].name, "ship-it");
        assert!(assets.warnings.iter().any(|w| w.contains("not [a-z0-9-]+")));
        assert!(assets.warnings.iter().any(|w| w.contains("empty body")));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn prompt_shadowing_a_builtin_is_skipped() {
        let root = fixture_root("shadow");
        std::fs::write(root.join("help.md"), "hijack the help\n").unwrap();
        std::fs::write(root.join("q.md"), "hijack the quit alias\n").unwrap();
        let m = manifest(&root, &[], &["help.md", "q.md"], &[]);
        let assets = assets_from_manifest(&root, &m);
        assert!(assets.commands.is_empty());
        assert_eq!(
            assets
                .warnings
                .iter()
                .filter(|w| w.contains("shadows a built-in"))
                .count(),
            2,
            "{:?}",
            assets.warnings
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_skipped() {
        let root = fixture_root("escape");
        let outside = fixture_root("escape-outside");
        std::fs::write(
            outside.join("evil.toml"),
            "[servers.evil]\ncommand = \"sh\"\n",
        )
        .unwrap();
        std::os::unix::fs::symlink(outside.join("evil.toml"), root.join("link.toml")).unwrap();
        let m = manifest(&root, &["link.toml"], &[], &[]);
        let assets = assets_from_manifest(&root, &m);
        assert!(assets.mcp_servers.is_empty());
        assert!(
            assets.warnings.iter().any(|w| w.contains("escapes")),
            "{:?}",
            assets.warnings
        );
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[cfg(unix)]
    #[test]
    fn dot_slash_command_resolves_in_root_with_containment() {
        let root = fixture_root("cmd");
        std::fs::write(root.join("server.sh"), "#!/bin/sh\n").unwrap();
        std::fs::write(
            root.join("servers.toml"),
            "[servers.local]\ncommand = \"./server.sh\"\n[servers.gone]\ncommand = \"./missing.sh\"\n",
        )
        .unwrap();
        let m = manifest(&root, &["servers.toml"], &[], &[]);
        let assets = assets_from_manifest(&root, &m);
        assert!(
            assets.mcp_servers["local"].command.ends_with("server.sh"),
            "{}",
            assets.mcp_servers["local"].command
        );
        assert!(std::path::Path::new(&assets.mcp_servers["local"].command).is_absolute());
        assert!(!assets.mcp_servers.contains_key("gone"));
        assert!(assets.warnings.iter().any(|w| w.contains("missing.sh")));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn url_only_bundle_entry_loads_without_command_rewrite() {
        // An HTTP server entry has no command; the `./`-relative rewrite must
        // not touch it (or warn it away as a missing file).
        let root = fixture_root("url-only");
        std::fs::write(
            root.join("servers.toml"),
            "[servers.remote]\nurl = \"https://example.com/mcp\"\n",
        )
        .unwrap();
        let m = manifest(&root, &["servers.toml"], &[], &[]);
        let assets = assets_from_manifest(&root, &m);
        assert_eq!(
            assets.mcp_servers["remote"].url.as_deref(),
            Some("https://example.com/mcp")
        );
        assert!(assets.mcp_servers["remote"].command.is_empty());
        assert!(assets.warnings.is_empty(), "{:?}", assets.warnings);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn apply_lets_config_win_and_merges_the_rest() {
        let mut config = crate::app::Config::default();
        config.mcp_servers.insert(
            "shared".to_string(),
            McpServerConfig {
                command: "config-wins".to_string(),
                ..Default::default()
            },
        );
        let mut assets = PluginAssets::default();
        assets.mcp_servers.insert(
            "shared".to_string(),
            McpServerConfig {
                command: "plugin-loses".to_string(),
                ..Default::default()
            },
        );
        assets.mcp_servers.insert(
            "fresh".to_string(),
            McpServerConfig {
                command: "plugin-wins".to_string(),
                ..Default::default()
            },
        );
        assets.agent_types.insert(
            "scout".to_string(),
            AgentTypeConfig {
                tools: None,
                safety: Some("read_only".to_string()),
                preamble: None,
                model: None,
            },
        );
        let warnings = apply(&mut config, &assets);
        assert_eq!(config.mcp_servers["shared"].command, "config-wins");
        assert_eq!(config.mcp_servers["fresh"].command, "plugin-wins");
        assert!(config.agents.types.contains_key("scout"));
        assert!(
            warnings.iter().any(|w| w.contains("shadowed")),
            "{warnings:?}"
        );
    }

    #[test]
    fn merge_assets_is_first_wins_deterministic() {
        let mut acc = PluginAssets::default();
        let mut first = PluginAssets::default();
        first.mcp_servers.insert(
            "s".to_string(),
            McpServerConfig {
                command: "first".to_string(),
                ..Default::default()
            },
        );
        let mut second = PluginAssets::default();
        second.mcp_servers.insert(
            "s".to_string(),
            McpServerConfig {
                command: "second".to_string(),
                ..Default::default()
            },
        );
        merge_assets(&mut acc, first);
        merge_assets(&mut acc, second);
        assert_eq!(acc.mcp_servers["s"].command, "first");
        assert!(
            acc.warnings
                .iter()
                .any(|w| w.contains("more than one plugin"))
        );
    }
}
