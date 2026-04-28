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
    for plugin in store.plugins().list()? {
        if !plugin.enabled {
            continue;
        }
        let manifest: PluginManifest = serde_json::from_str(&plugin.manifest_json)?;
        let root = PathBuf::from(&plugin.source);
        for hook in manifest.hooks {
            let hook_path = root.join(&hook);
            anyhow::ensure!(
                hook_path.starts_with(&root),
                "plugin hook escapes root: {}",
                hook
            );
            if !hook_path.exists() {
                continue;
            }
            let mut child = Command::new(&hook_path)
                .env("MERMAID_HOOK_EVENT", event)
                .env("MERMAID_PLUGIN_NAME", &plugin.name)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .with_context(|| format!("failed to run plugin hook {}", hook_path.display()))?;
            if let Some(stdin) = child.stdin.as_mut() {
                stdin.write_all(serde_json::to_string(payload)?.as_bytes())?;
            }
            let status = child.wait()?;
            anyhow::ensure!(
                status.success(),
                "plugin hook {} failed with {}",
                hook_path.display(),
                status
            );
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
    let git_source = if source.starts_with("https://github.com/")
        || source.starts_with("git@github.com:")
        || source.ends_with(".git")
    {
        Some(source.to_string())
    } else if source.matches('/').count() == 1 && !source.starts_with('/') {
        Some(format!("https://github.com/{}.git", source))
    } else {
        None
    };
    let Some(git_source) = git_source else {
        return Ok(path.to_path_buf());
    };

    let dest = data_dir()?
        .join("plugins")
        .join("sources")
        .join(hex_lower(&Sha256::digest(git_source.as_bytes())));
    if dest.exists() {
        let _ = Command::new("git")
            .arg("-C")
            .arg(&dest)
            .arg("pull")
            .arg("--ff-only")
            .status();
    } else {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let status = Command::new("git")
            .arg("clone")
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
