use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{NewPluginInstall, PluginInstallRecord, RuntimeStore, data_dir};

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
    #[serde(default)]
    pub permissions: Vec<String>,
    #[serde(default)]
    pub prompts: Vec<String>,
    #[serde(default)]
    pub bin: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginPermissionPreview {
    pub name: String,
    pub declared_permissions: Vec<String>,
    pub permissions_toml: Option<toml::Value>,
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
        enabled: true,
        manifest_json,
    })?;
    write_plugin_lockfile()?;
    Ok(record)
}

pub fn plugin_permission_preview(path: &Path) -> Result<PluginPermissionPreview> {
    let (_manifest_path, root, manifest) = load_plugin_manifest(path)?;
    validate_plugin_manifest(&manifest, &root)?;
    let permissions_path = root.join("permissions.toml");
    let permissions_toml = if permissions_path.exists() {
        let raw = std::fs::read_to_string(&permissions_path)
            .with_context(|| format!("failed to read {}", permissions_path.display()))?;
        Some(toml::from_str(&raw)?)
    } else {
        None
    };
    Ok(PluginPermissionPreview {
        name: manifest.name,
        declared_permissions: manifest.permissions,
        permissions_toml,
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
    std::fs::write(&path, serde_json::to_vec_pretty(&plugins)?)?;
    Ok(path)
}

pub fn run_plugin_hooks(event: &str, payload: &serde_json::Value) -> Result<()> {
    let store = RuntimeStore::open_default()?;
    let payload_bytes = serde_json::to_string(payload)?.into_bytes();
    for plugin in store.plugins().list()? {
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
            if let Some(stdin) = child.stdin.as_mut() {
                let _ = stdin.write_all(&payload_bytes);
            }
            match child.wait() {
                Ok(status) if !status.success() => {
                    tracing::warn!(plugin = %plugin.name, hook = %hook, %status, "plugin hook failed");
                },
                Err(err) => {
                    tracing::warn!(plugin = %plugin.name, error = %err, "plugin hook wait failed");
                },
                _ => {},
            }
        }
    }
    Ok(())
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
        .join(hex_lower(&Sha256::digest(git_source.as_bytes())));
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

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
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
            permissions: vec![],
            prompts: vec![],
            bin: vec![],
        };
        assert!(validate_plugin_manifest(&manifest, &root).is_err());
    }
}
