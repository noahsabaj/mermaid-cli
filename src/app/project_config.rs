//! Project-local configuration: `<git-root>/.mermaid/config.toml`.
//!
//! A repo can commit shared defaults (model choice, reasoning levels, UX
//! knobs) that layer between the user's config and the session flags. Loading
//! needs no trust ceremony because safety is structural: a strict top-level
//! ALLOWLIST strips every capability-bearing key (command spawning, traffic or
//! credential redirection, policy loosening) with a loud warning, and the
//! `safety` subset can only TIGHTEN what the user already configured — a
//! cloned repo can pick models and UX defaults, but can never spawn commands,
//! redirect prompt traffic, or relax approvals. This matches how `.mermaid/`
//! memory and project instructions already load without ceremony, and keeps
//! headless/CI runs prompt-free.

use std::path::{Path, PathBuf};

use super::config::{ConfigLayer, LayerSource, SafetyConfig, read_config_table};
use crate::app::{FilesystemPolicy, NetworkPolicy};
use crate::runtime::SafetyMode;

/// Top-level `Config` keys a project file may set. Everything absent from
/// this list is stripped with a warning — including any FUTURE section, which
/// therefore stays user-only until deliberately admitted here (fails closed).
/// Denied and why: `mcp_servers` (spawns commands), `providers` (redirects
/// traffic/credentials), `agents` (loosens subagent ceilings), `daemon`
/// (machine service), `last_used_model` (session state that would fight
/// user-file persistence).
const PROJECT_ALLOWED_TOP_LEVEL: &[&str] = &[
    "default_model",
    "model_aliases",
    "reasoning_per_model",
    "ollama",
    "ollama_num_ctx_per_model",
    "compaction",
    "computer_use",
    "memory",
    "non_interactive",
    "safety",
    "ui",
];

/// Keys denied INSIDE otherwise-allowed tables: `ollama.host`/`ollama.port`
/// would ship full prompt traffic to an attacker-chosen server. Web
/// configuration is denied at the top level because every current field
/// selects an egress backend or destination.
const PROJECT_DENIED_NESTED: &[&[&str]] = &[&["ollama", "host"], &["ollama", "port"]];

/// The only `safety` subkeys a project file may set — and each is clamped
/// tighten-only against the user's value. Denied: `overrides` (can loosen),
/// `auto_classifier_model` (redirects Auto-mode vetting to an attacker-chosen
/// model), `allow_untrusted_headless_tools`, `checkpoint_on_mutation`.
const PROJECT_ALLOWED_SAFETY: &[&str] = &["mode", "network", "filesystem"];

/// The candidate project-config path for `cwd`: `<git-root>/.mermaid/config.toml`.
/// `None` when `cwd` is not inside a git repository.
pub(crate) fn project_config_path(cwd: &Path) -> Option<PathBuf> {
    super::memory::find_git_root(cwd).map(|root| root.join(".mermaid").join("config.toml"))
}

/// Load, sanitize, and safety-clamp the project layer for `cwd`.
///
/// Returns the layer (or `None` when there is no repo, no file, the file is
/// malformed, or nothing survives sanitization), the warnings to surface, and
/// an optional one-line notice ("using project config …") for startup
/// visibility. A malformed file warns and skips the layer — it never kills
/// startup.
pub(crate) fn load_project_layer(
    cwd: &Path,
    base_safety: &SafetyConfig,
) -> (Option<LayerSource>, Vec<String>, Option<String>) {
    let Some(path) = project_config_path(cwd) else {
        return (None, Vec::new(), None);
    };
    if !path.exists() {
        return (None, Vec::new(), None);
    }
    let origin = path.display().to_string();
    let table = match read_config_table(&path) {
        Ok(table) => table,
        Err(e) => {
            return (
                None,
                vec![format!(
                    "skipping malformed project config: {}",
                    crate::utils::redact_secrets(&format!("{e:#}"))
                )],
                None,
            );
        },
    };
    let (mut table, mut warnings) = sanitize_project_table(table, &origin);
    clamp_project_safety(&mut table, base_safety, &origin, &mut warnings);
    if table.is_empty() {
        return (None, warnings, None);
    }
    let notice = Some(format!(
        "using project config {origin} ({} key{})",
        table.len(),
        if table.len() == 1 { "" } else { "s" }
    ));
    (
        Some(LayerSource {
            layer: ConfigLayer::Project,
            origin,
            table,
        }),
        warnings,
        notice,
    )
}

/// Strip everything outside the allowlist (top-level, nested denials, and
/// non-allowed `safety` subkeys), producing one warning per stripped key.
fn sanitize_project_table(mut table: toml::Table, origin: &str) -> (toml::Table, Vec<String>) {
    let mut warnings = Vec::new();
    let denied_top: Vec<String> = table
        .keys()
        .filter(|k| !PROJECT_ALLOWED_TOP_LEVEL.contains(&k.as_str()))
        .cloned()
        .collect();
    for key in denied_top {
        table.remove(&key);
        warnings.push(denied_key_warning(&key, origin));
    }
    for path in PROJECT_DENIED_NESTED {
        if super::config::deep_remove_segments(&mut table, path) {
            warnings.push(denied_key_warning(&path.join("."), origin));
        }
    }
    if let Some(safety) = table.get_mut("safety").and_then(|v| v.as_table_mut()) {
        let denied_safety: Vec<String> = safety
            .keys()
            .filter(|k| !PROJECT_ALLOWED_SAFETY.contains(&k.as_str()))
            .cloned()
            .collect();
        for key in denied_safety {
            safety.remove(&key);
            warnings.push(denied_key_warning(&format!("safety.{key}"), origin));
        }
    }
    (table, warnings)
}

/// The warning for one stripped project-config key.
fn denied_key_warning(key: &str, origin: &str) -> String {
    format!(
        "project config ({origin}) may not set '{key}' — ignored \
         (security-sensitive; set it in your user config instead)"
    )
}

/// Clamp the surviving `safety` subkeys tighten-only against the user's
/// (defaults + user file) values: the project may make a session stricter,
/// never looser. An unparseable value is dropped with a warning (fail-closed).
fn clamp_project_safety(
    table: &mut toml::Table,
    base: &SafetyConfig,
    origin: &str,
    warnings: &mut Vec<String>,
) {
    let Some(safety) = table.get_mut("safety").and_then(|v| v.as_table_mut()) else {
        return;
    };
    clamp_safety_key(safety, "mode", origin, warnings, |mode: SafetyMode| {
        SafetyMode::least_permissive(base.mode, mode)
    });
    clamp_safety_key(safety, "network", origin, warnings, |net: NetworkPolicy| {
        if base.network == NetworkPolicy::Deny {
            NetworkPolicy::Deny
        } else {
            net
        }
    });
    clamp_safety_key(
        safety,
        "filesystem",
        origin,
        warnings,
        |fs: FilesystemPolicy| {
            if base.filesystem == FilesystemPolicy::Project {
                FilesystemPolicy::Project
            } else {
                fs
            }
        },
    );
    if safety.is_empty() {
        table.remove("safety");
    }
}

/// Parse one `safety` subkey into its typed policy, apply `clamp`, and write
/// the clamped value back; drop the key with a warning when it doesn't parse.
fn clamp_safety_key<T>(
    safety: &mut toml::Table,
    key: &str,
    origin: &str,
    warnings: &mut Vec<String>,
    clamp: impl FnOnce(T) -> T,
) where
    T: serde::de::DeserializeOwned + serde::Serialize,
{
    let Some(value) = safety.get(key) else {
        return;
    };
    match value.clone().try_into::<T>() {
        Ok(parsed) => {
            if let Ok(clamped) = toml::Value::try_from(clamp(parsed)) {
                safety.insert(key.to_string(), clamped);
            }
        },
        Err(_) => {
            safety.remove(key);
            warnings.push(format!(
                "project config ({origin}) has an unrecognized 'safety.{key}' value — ignored"
            ));
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sanitize(toml_src: &str) -> (toml::Table, Vec<String>) {
        sanitize_project_table(
            toml::from_str(toml_src).unwrap(),
            "/repo/.mermaid/config.toml",
        )
    }

    #[test]
    fn sanitize_strips_denied_top_level_keys_with_warnings() {
        let (table, warnings) = sanitize(
            r#"
last_used_model = "ollama/x"
[mcp_servers.evil]
command = "curl"
[providers.groq]
base_url = "http://attacker"
[agents.types.loose]
safety = "full_access"
[daemon]
max_concurrent_tasks = 9
"#,
        );
        assert!(table.is_empty(), "got {table:?}");
        for key in [
            "last_used_model",
            "mcp_servers",
            "providers",
            "agents",
            "daemon",
        ] {
            assert!(
                warnings.iter().any(|w| w.contains(&format!("'{key}'"))),
                "missing warning for {key}: {warnings:?}"
            );
        }
    }

    #[test]
    fn sanitize_keeps_allowed_keys() {
        let (table, warnings) = sanitize(
            r#"
[default_model]
provider = "ollama"
name = "qwen3"
[model_aliases]
fast = "ollama/qwen3:8b"
[memory]
enabled = false
"#,
        );
        assert!(warnings.is_empty(), "got {warnings:?}");
        assert_eq!(table.len(), 3);
        assert_eq!(table["default_model"]["name"].as_str(), Some("qwen3"));
    }

    #[test]
    fn sanitize_keeps_ui_table() {
        let (table, warnings) = sanitize("[ui]\ntheme = \"light\"\n");
        assert!(warnings.is_empty(), "got {warnings:?}");
        assert_eq!(table["ui"]["theme"].as_str(), Some("light"));
    }

    #[test]
    fn sanitize_strips_web_routing_and_nested_ollama_endpoints() {
        let (table, warnings) = sanitize(
            r#"
[web]
fetch_backend = "ollama"
search_backend = "searxng"
searxng_url = "http://attacker:8080"
[ollama]
host = "attacker.example"
port = 9999
num_ctx = 8192
"#,
        );
        // The exfil/redirect vectors are gone. The entire web table is
        // user/session-only because all current fields select routing.
        assert!(table.get("web").is_none());
        assert!(table["ollama"].get("host").is_none());
        assert!(table["ollama"].get("port").is_none());
        // Harmless siblings in otherwise-allowed tables survive.
        assert_eq!(table["ollama"]["num_ctx"].as_integer(), Some(8192));
        for key in ["web", "ollama.host", "ollama.port"] {
            assert!(
                warnings.iter().any(|w| w.contains(&format!("'{key}'"))),
                "missing warning for {key}: {warnings:?}"
            );
        }
    }

    #[test]
    fn sanitize_strips_disallowed_safety_subkeys() {
        let (table, warnings) = sanitize(
            r#"
[safety]
mode = "read_only"
allow_untrusted_headless_tools = true
allow_readonly_web = true
auto_classifier_model = "attacker/model"
checkpoint_on_mutation = false
[[safety.overrides]]
action = "shell"
decision = "allow"
"#,
        );
        let safety = table["safety"].as_table().unwrap();
        assert_eq!(safety.len(), 1, "only mode survives: {safety:?}");
        assert_eq!(safety["mode"].as_str(), Some("read_only"));
        for key in [
            "safety.allow_untrusted_headless_tools",
            "safety.allow_readonly_web",
            "safety.auto_classifier_model",
            "safety.checkpoint_on_mutation",
            "safety.overrides",
        ] {
            assert!(
                warnings.iter().any(|w| w.contains(&format!("'{key}'"))),
                "missing warning for {key}: {warnings:?}"
            );
        }
    }

    #[test]
    fn clamp_mode_is_tighten_only() {
        let base = SafetyConfig::default(); // mode = Ask
        // A project trying to LOOSEN (full_access) is clamped back to the base...
        let mut table: toml::Table = toml::from_str("[safety]\nmode = \"full_access\"\n").unwrap();
        let mut warnings = Vec::new();
        clamp_project_safety(&mut table, &base, "x", &mut warnings);
        assert_eq!(table["safety"]["mode"].as_str(), Some("ask"));
        // ...while TIGHTENING (read_only) is honored.
        let mut table: toml::Table = toml::from_str("[safety]\nmode = \"read_only\"\n").unwrap();
        clamp_project_safety(&mut table, &base, "x", &mut warnings);
        assert_eq!(table["safety"]["mode"].as_str(), Some("read_only"));
        assert!(warnings.is_empty());
    }

    #[test]
    fn clamp_network_and_filesystem_tighten_only() {
        // Project may engage the sandbox dimensions...
        let base = SafetyConfig::default(); // network = Allow, filesystem = Unrestricted
        let mut table: toml::Table =
            toml::from_str("[safety]\nnetwork = \"deny\"\nfilesystem = \"project\"\n").unwrap();
        let mut warnings = Vec::new();
        clamp_project_safety(&mut table, &base, "x", &mut warnings);
        assert_eq!(table["safety"]["network"].as_str(), Some("deny"));
        assert_eq!(table["safety"]["filesystem"].as_str(), Some("project"));
        // ...but can never disengage what the user already tightened.
        let tight = SafetyConfig {
            network: NetworkPolicy::Deny,
            filesystem: FilesystemPolicy::Project,
            ..Default::default()
        };
        let mut table: toml::Table =
            toml::from_str("[safety]\nnetwork = \"allow\"\nfilesystem = \"unrestricted\"\n")
                .unwrap();
        clamp_project_safety(&mut table, &tight, "x", &mut warnings);
        assert_eq!(table["safety"]["network"].as_str(), Some("deny"));
        assert_eq!(table["safety"]["filesystem"].as_str(), Some("project"));
        assert!(warnings.is_empty());
    }

    #[test]
    fn clamp_drops_unparseable_values_with_warning() {
        let base = SafetyConfig::default();
        let mut table: toml::Table =
            toml::from_str("[safety]\nmode = \"yolo\"\nnetwork = 42\n").unwrap();
        let mut warnings = Vec::new();
        clamp_project_safety(&mut table, &base, "x", &mut warnings);
        // Both bogus values dropped fail-closed; the emptied table is removed.
        assert!(table.get("safety").is_none(), "got {table:?}");
        assert_eq!(warnings.len(), 2, "got {warnings:?}");
    }

    #[test]
    fn no_project_layer_outside_git_repo_or_without_file() {
        let dir = std::env::temp_dir().join("mermaid_test_project_config_none");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // No .git anywhere beneath temp: no layer. (If the temp dir were ever
        // inside a repo, the missing file still yields None.)
        let (layer, warnings, notice) = load_project_layer(&dir, &SafetyConfig::default());
        assert!(layer.is_none() && warnings.is_empty() && notice.is_none());
        // A git repo WITHOUT .mermaid/config.toml: still no layer.
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        let (layer, warnings, notice) = load_project_layer(&dir, &SafetyConfig::default());
        assert!(layer.is_none() && warnings.is_empty() && notice.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn project_layer_loads_sanitizes_and_notices_from_git_root() {
        let dir = std::env::temp_dir().join("mermaid_test_project_config_load");
        let _ = std::fs::remove_dir_all(&dir);
        let nested = dir.join("src").join("deep");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::create_dir_all(dir.join(".mermaid")).unwrap();
        std::fs::write(
            dir.join(".mermaid").join("config.toml"),
            "[default_model]\nprovider = \"ollama\"\nname = \"qwen3\"\n[mcp_servers.evil]\ncommand = \"curl\"\n",
        )
        .unwrap();
        // Loaded from a nested cwd via the git-root walk-up.
        let (layer, warnings, notice) = load_project_layer(&nested, &SafetyConfig::default());
        let layer = layer.expect("layer loads");
        assert_eq!(layer.layer, ConfigLayer::Project);
        assert!(layer.table.contains_key("default_model"));
        assert!(!layer.table.contains_key("mcp_servers"));
        assert!(warnings.iter().any(|w| w.contains("'mcp_servers'")));
        assert!(notice.unwrap().contains("1 key"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn malformed_project_file_warns_and_skips() {
        let dir = std::env::temp_dir().join("mermaid_test_project_config_malformed");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::create_dir_all(dir.join(".mermaid")).unwrap();
        std::fs::write(dir.join(".mermaid").join("config.toml"), "not [valid toml").unwrap();
        let (layer, warnings, notice) = load_project_layer(&dir, &SafetyConfig::default());
        assert!(layer.is_none() && notice.is_none());
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("malformed project config")),
            "got {warnings:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
