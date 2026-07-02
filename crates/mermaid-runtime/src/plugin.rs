use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{NewPluginInstall, PluginInstallRecord, RuntimeStore, data_dir};

/// A plugin hook that runs longer than this is killed. Hooks are fire-and-forget
/// observers, so a runaway one must never hang the caller (the TUI event loop,
/// the daemon, or the CLI) — this bound is what makes that guarantee.
const HOOK_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginManifest {
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub agents: Vec<String>,
    #[serde(default)]
    pub hooks: Vec<String>,
    #[serde(default)]
    pub mcp: Vec<String>,
    /// Capabilities the plugin *declares* it uses (e.g. "network", "filesystem").
    /// ADVISORY ONLY — surfaced at install/enable time for informed consent, not
    /// enforced. A plugin hook is a native child process running with the user's
    /// privileges; the runtime cannot confine it to this list without OS-level
    /// sandboxing, so the field documents intent rather than granting a sandbox.
    /// The real boundary is the explicit `mermaid plugin enable` decision.
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub prompts: Vec<String>,
    #[serde(default)]
    pub bin: Vec<String>,
}

/// A summary of what a plugin declares it will do, shown before install/enable.
/// These are advisory disclosures for informed consent, not an enforced sandbox
/// (see [`PluginManifest::capabilities`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginCapabilityPreview {
    pub name: String,
    pub declared_capabilities: Vec<String>,
    pub capabilities_toml: Option<toml::Value>,
    pub hooks: Vec<String>,
    pub mcp: Vec<String>,
    pub bin: Vec<String>,
}

pub fn validate_plugin_manifest(manifest: &PluginManifest, root: &Path) -> Result<()> {
    anyhow::ensure!(!manifest.name.trim().is_empty(), "plugin name is required");
    ensure_relative_paths("skills", &manifest.skills, root)?;
    ensure_relative_paths("agents", &manifest.agents, root)?;
    ensure_relative_paths("hooks", &manifest.hooks, root)?;
    ensure_relative_paths("mcp", &manifest.mcp, root)?;
    ensure_relative_paths("prompts", &manifest.prompts, root)?;
    ensure_relative_paths("bin", &manifest.bin, root)?;
    Ok(())
}

pub fn install_plugin_from_path(path: &Path) -> Result<PluginInstallRecord> {
    let (_manifest_path, root, manifest) = load_plugin_manifest(path)?;
    validate_plugin_manifest(&manifest, &root)?;
    let manifest_json = serde_json::to_string_pretty(&manifest)?;
    let store = RuntimeStore::open_default()?;
    let record = store.plugins().install(NewPluginInstall {
        id: Some(manifest.name.clone()),
        name: manifest.name,
        source: root.display().to_string(),
        version: manifest.version,
        // Installed plugins are DISABLED by default. A hook runs native code
        // with the user's privileges, so activation is a separate, explicit
        // decision (`mermaid plugin enable <id>`) — never a side effect of
        // install. This is the meaningful boundary in a hook system.
        enabled: false,
        manifest_json,
    })?;
    write_plugin_lockfile()?;
    Ok(record)
}

pub fn plugin_capability_preview(path: &Path) -> Result<PluginCapabilityPreview> {
    let (_manifest_path, root, manifest) = load_plugin_manifest(path)?;
    validate_plugin_manifest(&manifest, &root)?;
    let capabilities_path = root.join("capabilities.toml");
    let capabilities_toml = if capabilities_path.exists() {
        let raw = std::fs::read_to_string(&capabilities_path)
            .with_context(|| format!("failed to read {}", capabilities_path.display()))?;
        Some(toml::from_str(&raw)?)
    } else {
        None
    };
    Ok(PluginCapabilityPreview {
        name: manifest.name,
        declared_capabilities: manifest.capabilities,
        capabilities_toml,
        hooks: manifest.hooks,
        mcp: manifest.mcp,
        bin: manifest.bin,
    })
}

pub fn write_plugin_lockfile() -> Result<PathBuf> {
    let store = RuntimeStore::open_default()?;
    let plugins = store.plugins().list()?;
    let path = data_dir()?.join("plugins.lock.json");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Atomic write so a crash can't leave a truncated lockfile.
    crate::write_atomic(&path, &serde_json::to_vec_pretty(&plugins)?)?;
    Ok(path)
}

pub fn run_plugin_hooks(event: &str, payload: &serde_json::Value) -> Result<()> {
    let store = RuntimeStore::open_default()?;
    let payload_bytes = std::sync::Arc::new(serde_json::to_string(payload)?.into_bytes());
    for plugin in store.plugins().list()? {
        // The enabled flag is the trust boundary: a plugin runs native hook
        // code only after an explicit `plugin enable`. Declared capabilities are
        // advisory and intentionally not consulted here — they cannot constrain
        // a native child process.
        if !plugin.enabled {
            continue;
        }
        let Ok(manifest) = serde_json::from_str::<PluginManifest>(&plugin.manifest_json) else {
            tracing::warn!(plugin = %plugin.name, "skipping plugin with unparseable manifest");
            continue;
        };
        // Canonicalize the root so a symlink inside it can't be used to escape.
        let Ok(root) = std::fs::canonicalize(&plugin.source) else {
            tracing::warn!(plugin = %plugin.name, "plugin source missing; skipping hooks");
            continue;
        };
        for hook in manifest.hooks {
            // Resolve the hook through symlinks and verify containment on the
            // CANONICAL path (the old lexical `starts_with` could be escaped
            // by a symlink inside the root pointing outside it).
            let Ok(canonical_hook) = std::fs::canonicalize(root.join(&hook)) else {
                continue; // missing hook: nothing to run
            };
            if !canonical_hook.starts_with(&root) {
                tracing::warn!(plugin = %plugin.name, hook = %hook, "plugin hook escapes root; skipping");
                continue;
            }
            // Execute with a SCRUBBED environment (clear + minimal allowlist)
            // so provider API keys and MERMAID_DAEMON_TOKEN never leak into
            // plugin-provided code.
            let spawn = Command::new(&canonical_hook)
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap_or_default())
                .env("HOME", std::env::var_os("HOME").unwrap_or_default())
                .env("MERMAID_HOOK_EVENT", event)
                .env("MERMAID_PLUGIN_NAME", &plugin.name)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
            let mut child = match spawn {
                Ok(child) => child,
                Err(err) => {
                    tracing::warn!(plugin = %plugin.name, error = %err, "failed to spawn plugin hook");
                    continue; // isolate: one bad hook must not abort the rest
                },
            };
            // Write the payload on a detached thread, then let stdin drop so the
            // hook sees EOF. Two failure modes are bounded here: a hook that
            // reads stdin-to-EOF (the drop unblocks it), AND a hook that never
            // reads stdin while the payload exceeds the pipe buffer (~64 KiB;
            // checkpoint payloads embed the full file list) — a synchronous
            // `write_all` would block forever there, and the timeout below only
            // bounds the WAIT. The thread unblocks when the child exits or is
            // killed on timeout (closing the pipe), so we never join it.
            if let Some(mut stdin) = child.stdin.take() {
                let payload = std::sync::Arc::clone(&payload_bytes);
                std::thread::spawn(move || {
                    let _ = stdin.write_all(&payload);
                });
            }
            wait_hook_bounded(&mut child, &plugin.name, &hook, HOOK_TIMEOUT);
        }
    }
    Ok(())
}

/// Wait for a plugin hook to exit, killing it if it overruns `timeout`.
/// Synchronous — the runtime crate has no async runtime; callers that must not
/// block an executor (the `effect/` loop) wrap `run_plugin_hooks` in
/// `spawn_blocking`.
fn wait_hook_bounded(child: &mut std::process::Child, plugin: &str, hook: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    tracing::warn!(plugin = %plugin, hook = %hook, %status, "plugin hook failed");
                }
                return;
            },
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    tracing::warn!(plugin = %plugin, hook = %hook, "plugin hook timed out; killed");
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            },
            Err(err) => {
                tracing::warn!(plugin = %plugin, error = %err, "plugin hook wait failed");
                return;
            },
        }
    }
}

fn load_plugin_manifest(path: &Path) -> Result<(PathBuf, PathBuf, PluginManifest)> {
    let resolved = resolve_plugin_source(path)?;
    let manifest_path = if resolved.is_dir() {
        resolved.join("plugin.toml")
    } else {
        resolved
    };
    let root = manifest_path
        .parent()
        .context("plugin manifest must have a parent directory")?
        .to_path_buf();
    let raw = std::fs::read_to_string(&manifest_path)
        .with_context(|| format!("failed to read {}", manifest_path.display()))?;
    let manifest: PluginManifest = toml::from_str(&raw)
        .with_context(|| format!("failed to parse {}", manifest_path.display()))?;
    Ok((manifest_path, root, manifest))
}

fn resolve_plugin_source(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        return Ok(path.to_path_buf());
    }
    let source = path.to_string_lossy();
    // Only an EXPLICIT git URL is treated as a remote source. The old bare
    // `owner/repo` → github.com expansion turned any short string into a
    // network fetch of attacker-named code; require the full URL instead.
    let is_git_url = source.starts_with("https://")
        || source.starts_with("git@")
        || source.starts_with("ssh://")
        || source.ends_with(".git");
    if !is_git_url {
        return Ok(path.to_path_buf());
    }

    // Fetching remote plugin code is a privileged operation — gate it behind
    // an explicit opt-in so it can't be triggered silently (e.g. via the
    // daemon's local fallback). Operators who want it set the env var.
    anyhow::ensure!(
        std::env::var("MERMAID_ALLOW_PLUGIN_FETCH").is_ok_and(|v| v == "1" || v == "true"),
        "refusing to fetch remote plugin source {source:?}: set MERMAID_ALLOW_PLUGIN_FETCH=1 to allow, \
         or clone it yourself and install from the local path",
    );

    let git_source = source.to_string();
    let dest = data_dir()?
        .join("plugins")
        .join("sources")
        .join(crate::hex_lower(&Sha256::digest(git_source.as_bytes())));
    // Harden git: no credential prompts, no repo-provided hooks, no external
    // transports (`ext::` RCE), so materializing the source can't itself run
    // attacker code.
    const HARDENING: [&str; 4] = [
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "protocol.ext.allow=never",
    ];
    if dest.exists() {
        let _ = Command::new("git")
            .env("GIT_TERMINAL_PROMPT", "0")
            .args(HARDENING)
            .arg("-C")
            .arg(&dest)
            .args(["pull", "--ff-only"])
            .status();
    } else {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let status = Command::new("git")
            .env("GIT_TERMINAL_PROMPT", "0")
            .args(HARDENING)
            .args(["clone", "--depth", "1"])
            .arg(&git_source)
            .arg(&dest)
            .status()
            .with_context(|| format!("failed to clone plugin source {}", git_source))?;
        anyhow::ensure!(status.success(), "git clone failed for {}", git_source);
    }
    Ok(dest)
}

fn ensure_relative_paths(kind: &str, paths: &[String], root: &Path) -> Result<()> {
    for path in paths {
        let rel = Path::new(path);
        anyhow::ensure!(
            !rel.is_absolute() && !path.contains(".."),
            "{} path must stay inside plugin root: {}",
            kind,
            path
        );
        let full = root.join(rel);
        anyhow::ensure!(
            full.exists(),
            "{} path does not exist under plugin root: {}",
            kind,
            path
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::*;

    #[test]
    fn manifest_rejects_parent_escape() {
        let root = std::env::temp_dir();
        let manifest = PluginManifest {
            name: "bad".to_string(),
            version: None,
            description: None,
            skills: vec!["../x".to_string()],
            agents: vec![],
            hooks: vec![],
            mcp: vec![],
            capabilities: vec![],
            prompts: vec![],
            bin: vec![],
        };
        assert!(validate_plugin_manifest(&manifest, &root).is_err());
    }

    #[test]
    fn manifest_round_trips_capabilities_field() {
        let toml_src = r#"
            name = "demo"
            capabilities = ["network", "filesystem"]
        "#;
        let manifest: PluginManifest = toml::from_str(toml_src).expect("parse manifest");
        assert_eq!(manifest.capabilities, vec!["network", "filesystem"]);
        // Serializes back under the new key, and the old key is gone.
        let json = serde_json::to_string(&manifest).expect("serialize");
        assert!(json.contains("\"capabilities\""));
        assert!(!json.contains("\"permissions\""));
    }

    #[test]
    fn hook_overrunning_timeout_is_killed() {
        use std::time::{Duration, Instant};
        // A hook that sleeps far past the timeout must be killed, and
        // wait_hook_bounded must return promptly (no permanent hang).
        #[cfg(unix)]
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("sleep 10")
            .spawn()
            .expect("spawn sleep");
        #[cfg(windows)]
        let mut child = std::process::Command::new("cmd")
            .args(["/C", "ping -n 11 127.0.0.1 >NUL"])
            .spawn()
            .expect("spawn ping");
        let start = Instant::now();
        super::wait_hook_bounded(&mut child, "test", "hook", Duration::from_millis(150));
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "should return promptly after killing the overrunning hook"
        );
    }

    #[test]
    fn hook_that_exits_quickly_returns_without_kill() {
        use std::time::{Duration, Instant};
        #[cfg(unix)]
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn true");
        #[cfg(windows)]
        let mut child = std::process::Command::new("cmd")
            .args(["/C", "exit 0"])
            .spawn()
            .expect("spawn exit");
        let start = Instant::now();
        super::wait_hook_bounded(&mut child, "test", "hook", Duration::from_secs(30));
        assert!(start.elapsed() < Duration::from_secs(5));
    }
}
