use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{NewApproval, NewCheckpoint, RuntimeStore, data_dir};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointFile {
    pub path: String,
    pub existed: bool,
    pub snapshot_relpath: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointManifest {
    pub id: String,
    #[serde(default)]
    pub task_id: Option<String>,
    pub project_path: String,
    pub files: Vec<CheckpointFile>,
    pub pending_action: Option<serde_json::Value>,
    #[serde(default)]
    pub shadow_git_repo: Option<String>,
    #[serde(default)]
    pub shadow_git_commit: Option<String>,
    pub created_at: String,
}

pub fn create_checkpoint(
    project_path: &Path,
    paths: &[PathBuf],
    pending_action: Option<serde_json::Value>,
) -> Result<CheckpointManifest> {
    create_checkpoint_for_task(project_path, paths, pending_action, None)
}

pub fn create_checkpoint_for_task(
    project_path: &Path,
    paths: &[PathBuf],
    pending_action: Option<serde_json::Value>,
    task_id: Option<String>,
) -> Result<CheckpointManifest> {
    let id = fresh_checkpoint_id();
    let root = data_dir()?.join("checkpoints").join(&id);
    let files_dir = root.join("files");
    std::fs::create_dir_all(&files_dir)
        .with_context(|| format!("failed to create checkpoint dir {}", files_dir.display()))?;

    let project_root = std::fs::canonicalize(project_path).unwrap_or_else(|_| project_path.into());
    let mut files = Vec::new();
    for path in paths {
        let candidate = if path.is_absolute() {
            path.clone()
        } else {
            project_path.join(path)
        };
        let normalized = std::fs::canonicalize(&candidate).unwrap_or(candidate.clone());
        let display = normalized
            .strip_prefix(&project_root)
            .unwrap_or(&normalized)
            .display()
            .to_string();
        if normalized.exists() && normalized.is_file() {
            let safe_rel = sanitize_relpath(&display);
            let dest = files_dir.join(&safe_rel);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&normalized, &dest).with_context(|| {
                format!(
                    "failed to copy checkpoint file {} -> {}",
                    normalized.display(),
                    dest.display()
                )
            })?;
            files.push(CheckpointFile {
                path: display,
                existed: true,
                snapshot_relpath: Some(format!("files/{}", safe_rel)),
            });
        } else {
            files.push(CheckpointFile {
                path: display,
                existed: false,
                snapshot_relpath: None,
            });
        }
    }

    let shadow_git = snapshot_shadow_git(&project_root, &files, &id).ok();
    let manifest = CheckpointManifest {
        id: id.clone(),
        task_id: task_id.clone(),
        project_path: project_path.display().to_string(),
        files,
        pending_action,
        shadow_git_repo: shadow_git.as_ref().map(|snapshot| snapshot.repo.clone()),
        shadow_git_commit: shadow_git.as_ref().map(|snapshot| snapshot.commit.clone()),
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    let manifest_path = root.join("manifest.json");
    std::fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest)?)?;

    if let Ok(store) = RuntimeStore::open_default() {
        let _ = store.checkpoints().create(NewCheckpoint {
            id: Some(id),
            task_id,
            project_path: manifest.project_path.clone(),
            snapshot_path: root.display().to_string(),
            changed_files_json: serde_json::to_string(&manifest.files)?,
            pending_action_json: manifest
                .pending_action
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
            approval_id: None,
        });
    }

    let _ = crate::run_plugin_hooks(
        "checkpoint",
        &serde_json::json!({
            "id": manifest.id.clone(),
            "task_id": manifest.task_id.clone(),
            "project_path": manifest.project_path.clone(),
            "files": manifest.files.clone(),
            "created_at": manifest.created_at.clone(),
        }),
    );

    Ok(manifest)
}

pub fn restore_checkpoint(id: &str) -> Result<CheckpointManifest> {
    let root = data_dir()?.join("checkpoints").join(id);
    let manifest_path = root.join("manifest.json");
    let raw = std::fs::read_to_string(&manifest_path)
        .with_context(|| format!("failed to read {}", manifest_path.display()))?;
    let manifest: CheckpointManifest = serde_json::from_str(&raw)?;
    let project_root = PathBuf::from(&manifest.project_path);
    for file in &manifest.files {
        let target = project_root.join(&file.path);
        if file.existed {
            let rel = file
                .snapshot_relpath
                .as_ref()
                .context("checkpoint file missing snapshot_relpath")?;
            let source = root.join(rel);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&source, &target).with_context(|| {
                format!(
                    "failed to restore checkpoint file {} -> {}",
                    source.display(),
                    target.display()
                )
            })?;
        } else if target.exists() {
            if target.is_file() {
                std::fs::remove_file(&target)?;
            } else if target.is_dir() {
                std::fs::remove_dir_all(&target)?;
            }
        }
    }
    if let Some(action) = manifest.pending_action.as_ref()
        && action.get("tool").is_some()
        && let Ok(store) = RuntimeStore::open_default()
    {
        let proposed_action = action
            .get("tool")
            .and_then(|value| value.as_str())
            .unwrap_or("restored action")
            .to_string();
        let pending_action_json = serde_json::to_string(action).ok();
        if let Ok(approval) = store.approvals().create(NewApproval {
            task_id: manifest.task_id.clone(),
            proposed_action: format!("restore replay: {}", proposed_action),
            risk_classification: "restored_action".to_string(),
            policy_decision: "ask".to_string(),
            args_summary: pending_action_json.clone(),
            checkpoint_id: Some(manifest.id.clone()),
            pending_action_json,
        }) {
            let _ = store.checkpoints().set_approval(&manifest.id, &approval.id);
        }
    }
    Ok(manifest)
}

fn sanitize_relpath(path: &str) -> String {
    path.split(std::path::MAIN_SEPARATOR)
        .flat_map(|part| part.split('/'))
        .filter(|part| !part.is_empty() && *part != "." && *part != "..")
        .collect::<Vec<_>>()
        .join("__")
}

struct ShadowGitSnapshot {
    repo: String,
    commit: String,
}

fn snapshot_shadow_git(
    project_root: &Path,
    files: &[CheckpointFile],
    checkpoint_id: &str,
) -> Result<ShadowGitSnapshot> {
    let repo_root = data_dir()?
        .join("shadow-git")
        .join(project_hash(project_root));
    let worktree = repo_root.join("worktree");
    std::fs::create_dir_all(&worktree)?;
    if !worktree.join(".git").exists() {
        run_git(&worktree, ["init"])?;
    }

    run_git(&worktree, ["config", "user.name", "Mermaid Checkpoints"])?;
    run_git(
        &worktree,
        ["config", "user.email", "mermaid-checkpoints@localhost"],
    )?;

    for file in files {
        let shadow_path = worktree.join(&file.path);
        let project_path = project_root.join(&file.path);
        if file.existed && project_path.is_file() {
            if let Some(parent) = shadow_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&project_path, &shadow_path).with_context(|| {
                format!(
                    "failed to update shadow checkpoint {} -> {}",
                    project_path.display(),
                    shadow_path.display()
                )
            })?;
        } else if shadow_path.exists() {
            if shadow_path.is_dir() {
                std::fs::remove_dir_all(&shadow_path)?;
            } else {
                std::fs::remove_file(&shadow_path)?;
            }
        }
    }

    run_git(&worktree, ["add", "-A"])?;
    let status = Command::new("git")
        .arg("diff")
        .arg("--cached")
        .arg("--quiet")
        .current_dir(&worktree)
        .status()?;
    if !status.success() {
        run_git_with_env(
            &worktree,
            ["commit", "-m", &format!("checkpoint {checkpoint_id}")],
        )?;
    }
    let commit =
        git_output(&worktree, ["rev-parse", "HEAD"]).unwrap_or_else(|_| "uncommitted".to_string());
    Ok(ShadowGitSnapshot {
        repo: worktree.display().to_string(),
        commit,
    })
}

fn run_git<const N: usize>(cwd: &Path, args: [&str; N]) -> Result<()> {
    run_git_with_env(cwd, args)
}

fn run_git_with_env<const N: usize>(cwd: &Path, args: [&str; N]) -> Result<()> {
    let status = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "Mermaid Checkpoints")
        .env("GIT_AUTHOR_EMAIL", "mermaid-checkpoints@localhost")
        .env("GIT_COMMITTER_NAME", "Mermaid Checkpoints")
        .env("GIT_COMMITTER_EMAIL", "mermaid-checkpoints@localhost")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    anyhow::ensure!(
        status.success(),
        "shadow git command failed in {}",
        cwd.display()
    );
    Ok(())
}

fn git_output<const N: usize>(cwd: &Path, args: [&str; N]) -> Result<String> {
    let output = Command::new("git").args(args).current_dir(cwd).output()?;
    anyhow::ensure!(output.status.success(), "shadow git command failed");
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn project_hash(path: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(path.display().to_string().as_bytes());
    hex_lower(&hasher.finalize())
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

fn fresh_checkpoint_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("checkpoint-{nanos:x}")
}

#[cfg(test)]
mod tests {
    use crate::*;

    #[test]
    fn checkpoint_restore_round_trips_file_and_created_file() {
        let root = std::env::temp_dir().join("mermaid_checkpoint_test");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.txt"), "before").unwrap();
        let manifest = create_checkpoint(
            &root,
            &[root.join("a.txt"), root.join("new.txt")],
            Some(serde_json::json!({"tool": "write_file"})),
        )
        .unwrap();
        std::fs::write(root.join("a.txt"), "after").unwrap();
        std::fs::write(root.join("new.txt"), "created").unwrap();
        let restored = restore_checkpoint(&manifest.id).unwrap();
        assert_eq!(restored.id, manifest.id);
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "before"
        );
        assert!(!root.join("new.txt").exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
