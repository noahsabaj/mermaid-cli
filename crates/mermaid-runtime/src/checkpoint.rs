use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::pathguard::{contain_within, contain_within_canonical};
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
    // Collision-hardened id (salt+seq+nanos) — the old time-only id could repeat
    // within a coarse-clock tick and overwrite a prior checkpoint's files (#117).
    let id = crate::storage::fresh_id("checkpoint");
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
    // Atomic write: a crash mid-write must not leave a half-written manifest —
    // restore depends on it parsing cleanly.
    crate::write_atomic(&manifest_path, &serde_json::to_vec_pretty(&manifest)?)?;

    if let Ok(store) = RuntimeStore::open_default() {
        // Don't swallow the insert error (#117): a failed insert means the
        // manifest+files are on disk but the DB has no row, so a later restore
        // can't find them. Roll the on-disk checkpoint back and surface it.
        if let Err(error) = store.checkpoints().create(NewCheckpoint {
            id: Some(id.clone()),
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
        }) {
            let _ = std::fs::remove_dir_all(&root);
            return Err(error)
                .with_context(|| format!("failed to record checkpoint {id} in the runtime DB"));
        }
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
    // Confine the checkpoint id to the checkpoints dir: reject `..`/absolute
    // traversal that would read a manifest from anywhere on disk.
    let checkpoints_dir = data_dir()?.join("checkpoints");
    let ckpt_dir = contain_within(&checkpoints_dir, id)
        .with_context(|| format!("invalid checkpoint id: {id:?}"))?;
    let manifest_path = ckpt_dir.join("manifest.json");
    let raw = std::fs::read_to_string(&manifest_path)
        .with_context(|| format!("failed to read {}", manifest_path.display()))?;
    let manifest: CheckpointManifest = serde_json::from_str(&raw)?;
    // The confinement root must be a trusted, sane project directory — never a
    // value the (tamperable) manifest can redirect to `/` or a system dir.
    let project_root = resolve_restore_root(id, &manifest)?;

    // Plan the restore as two ordered phases so a mid-way failure can't leave a
    // half-applied tree: validate + collect every write and delete first (recording
    // each snapshot's validated SOURCE PATH, not its bytes — F71), then apply all
    // writes (each reads one snapshot and writes it atomically) and only then the
    // deletes. Prior state is moved aside into a staging dir (F72), so on any error
    // we roll the applied ops back best-effort — including non-empty directories —
    // instead of returning with the project half-restored.
    let mut writes: Vec<RestoreOp> = Vec::new();
    let mut deletes: Vec<RestoreOp> = Vec::new();
    for file in &manifest.files {
        // The manifest is on-disk state a tampered or shared checkpoint could
        // have rewritten. Confine every restore target to the recorded project
        // root — rejecting absolute paths, `..` escapes, AND symlinks planted
        // inside the root. Anything that doesn't resolve inside the root is
        // skipped, not run.
        let target = match contain_within_canonical(&project_root, &file.path) {
            Ok(target) => target,
            Err(err) => {
                tracing::warn!(
                    path = %file.path,
                    error = %err,
                    "skipping checkpoint entry that escapes the project root"
                );
                continue;
            },
        };
        if file.existed {
            let rel = file
                .snapshot_relpath
                .as_ref()
                .context("checkpoint file missing snapshot_relpath")?;
            // The snapshot source is also a manifest-supplied string; confine it
            // to this checkpoint's own directory so a crafted `snapshot_relpath`
            // (`../../etc/passwd`) can't read an arbitrary file as the source.
            let source = match contain_within(&ckpt_dir, rel) {
                Ok(source) => source,
                Err(err) => {
                    tracing::warn!(
                        relpath = %rel,
                        error = %err,
                        "skipping checkpoint entry with an escaping snapshot_relpath"
                    );
                    continue;
                },
            };
            // Defer reading the snapshot until apply time (F71): the planner only
            // records the validated source PATH, so the restore holds at most one
            // file in memory at a time instead of every snapshot at once.
            writes.push(RestoreOp::Write { target, source });
        } else {
            deletes.push(RestoreOp::Delete { target });
        }
    }

    // Stage prior state inside the project root so displaced files/dirs are moved
    // (rename), not held in memory or deleted outright: same-filesystem keeps the
    // rename atomic, and a non-empty prior directory survives a rollback (F72). The
    // fresh, hidden name can't collide with a (already-resolved) restore target.
    let staging = project_root.join(format!(
        ".mermaid-restore.{}",
        crate::storage::fresh_id("restore")
    ));
    std::fs::create_dir_all(&staging)
        .with_context(|| format!("failed to create restore staging dir {}", staging.display()))?;

    let mut applied: Vec<PriorState> = Vec::new();
    if let Err(err) = apply_restore(&writes, &deletes, &staging, &mut applied) {
        rollback_restore(&applied);
        // Rollback renamed every staged item back out, so staging should now be
        // empty; remove it only if so (`remove_dir`), never force-deleting prior
        // data a partial rollback could not restore.
        let _ = std::fs::remove_dir(&staging);
        return Err(err.context(
            "checkpoint restore failed; changes already applied were rolled back (best-effort)",
        ));
    }
    // Commit: the restore stuck, so the staged prior copies are now garbage.
    let _ = std::fs::remove_dir_all(&staging);
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

/// One planned restore mutation. All writes are applied (atomically) before any
/// delete so a failure can't strand the tree in a half-applied state. A write
/// carries the validated snapshot SOURCE path (not its bytes); the bytes are read
/// one file at a time at apply time, so peak memory is bounded by the largest
/// single file rather than the whole checkpoint (F71).
enum RestoreOp {
    Write { target: PathBuf, source: PathBuf },
    Delete { target: PathBuf },
}

/// A target's prior state, captured for rollback. The displaced file or directory
/// subtree (when the target existed) was moved into the staging area via rename,
/// so rollback restores it by moving it back — no prior bytes are held in memory
/// and a non-empty directory is preserved in full (F71/F72).
struct PriorState {
    target: PathBuf,
    /// Staging path the prior file/dir was renamed to, or `None` if the target did
    /// not exist before the restore (rollback then just removes what we created).
    staged: Option<PathBuf>,
}

/// Move an existing target (file OR directory subtree) aside into `staging` via
/// rename, returning the staging path so rollback can move it back. `Ok(None)`
/// means the target did not exist — nothing to preserve. Rename keeps peak memory
/// flat: a large file or a whole subtree is moved, never read.
fn stage_prior(target: &Path, staging: &Path, counter: &mut usize) -> Result<Option<PathBuf>> {
    if !target.exists() {
        return Ok(None);
    }
    let dest = staging.join(counter.to_string());
    *counter += 1;
    std::fs::rename(target, &dest)
        .with_context(|| format!("failed to stage prior state of {}", target.display()))?;
    Ok(Some(dest))
}

/// Remove whatever currently sits at `path` (a freshly written file, or nothing),
/// tolerating files, directories, and symlinks. `symlink_metadata` does not follow
/// links, so a symlinked target is unlinked rather than its destination cleared.
fn remove_path(path: &Path) {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => {
            let _ = std::fs::remove_dir_all(path);
        },
        Ok(_) => {
            let _ = std::fs::remove_file(path);
        },
        Err(_) => {},
    }
}

/// Apply writes (each via the atomic temp+rename writer) then deletes. Prior state
/// is moved aside into `staging` (rename) and recorded in `applied` so the caller
/// can roll back on error. Reads at most one snapshot file into memory at a time
/// (F71), and preserves a non-empty prior directory across rollback (F72).
fn apply_restore(
    writes: &[RestoreOp],
    deletes: &[RestoreOp],
    staging: &Path,
    applied: &mut Vec<PriorState>,
) -> Result<()> {
    let mut counter = 0usize;
    for op in writes {
        if let RestoreOp::Write { target, source } = op {
            // Read just THIS snapshot (bounded by one file) BEFORE displacing the
            // target, so a missing/unreadable source fails without moving the prior
            // file aside (F71).
            let bytes = std::fs::read(source).with_context(|| {
                format!("failed to read checkpoint snapshot {}", source.display())
            })?;
            let staged = stage_prior(target, staging, &mut counter)?;
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            crate::write_atomic(target, &bytes).with_context(|| {
                format!("failed to restore checkpoint file {}", target.display())
            })?;
            applied.push(PriorState {
                target: target.clone(),
                staged,
            });
        }
    }
    for op in deletes {
        if let RestoreOp::Delete { target } = op
            && target.exists()
        {
            // Move the prior file/dir aside instead of deleting it outright, so a
            // later failure can roll a non-empty directory subtree back (F72).
            let staged = stage_prior(target, staging, &mut counter)?;
            applied.push(PriorState {
                target: target.clone(),
                staged,
            });
        }
    }
    Ok(())
}

/// Best-effort undo of the ops in `applied`, newest first: remove whatever the
/// restore put at each target, then move the staged prior file/directory back. A
/// non-empty prior directory is restored in full because it was moved aside
/// (rename) rather than deleted (F72).
fn rollback_restore(applied: &[PriorState]) {
    for prior in applied.iter().rev() {
        remove_path(&prior.target);
        if let Some(staged) = &prior.staged {
            if let Some(parent) = prior.target.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::rename(staged, &prior.target);
        }
    }
}

/// Resolve the trusted project root a checkpoint may restore into. Prefer the
/// DB-recorded `project_path` (written at create time) and require the manifest
/// to agree with it, so a manifest-only tamper is rejected. Either way the root
/// must be an absolute directory with at least one normal component — a bare
/// filesystem root (`/`, `C:\`) confines nothing, since every absolute path
/// `starts_with` it (the original escape primitive).
fn resolve_restore_root(id: &str, manifest: &CheckpointManifest) -> Result<PathBuf> {
    let recorded = RuntimeStore::open_default()
        .ok()
        .and_then(|store| store.checkpoints().get(id).ok().flatten())
        .map(|rec| rec.project_path);
    let root_str = match recorded {
        Some(db_path) => {
            anyhow::ensure!(
                db_path == manifest.project_path,
                "checkpoint project_path does not match the recorded root (tampered manifest?)"
            );
            db_path
        },
        None => manifest.project_path.clone(),
    };
    let root = PathBuf::from(&root_str);
    anyhow::ensure!(
        root.is_absolute() && root.components().any(|c| matches!(c, Component::Normal(_))),
        "unsafe checkpoint project root: {}",
        root.display()
    );
    Ok(root)
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
        // `file.path` is the project-root-relative display path for in-tree
        // files, but an ABSOLUTE path for anything `strip_prefix(project_root)`
        // couldn't relativize (a file outside the project, a canonicalization
        // mismatch). `Path::join` with an absolute (or `..`-laden) component
        // escapes the worktree — `worktree.join("/etc/passwd") == "/etc/passwd"`
        // — and then `fs::copy(project_path, shadow_path)` below would be
        // `fs::copy(p, p)`, which truncates the real file to zero (std opens the
        // destination with truncate before reading the identical source), or the
        // `remove_dir_all` branch would delete a real directory. Only sync entries
        // that stay confined under the worktree; out-of-tree files are still
        // captured by the sanitized `files/` copy + manifest, and restore is
        // independently path-confined.
        let rel = Path::new(&file.path);
        if rel.is_absolute() || rel.components().any(|c| c == Component::ParentDir) {
            continue;
        }
        let shadow_path = worktree.join(rel);
        let project_path = project_root.join(rel);
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
        .args(HOOKS_OFF)
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

/// Disable repo-provided git hooks on every shadow-git invocation. The shadow
/// worktree lives under the data dir, but the project's own hooks (and a
/// tampered shadow repo) must never run as a side effect of taking a
/// checkpoint. A nonexistent hooks dir ⇒ git runs no hooks (works on Windows
/// Git too). Mirrors the `core.hooksPath=/dev/null` hardening in `plugin.rs`.
const HOOKS_OFF: [&str; 2] = ["-c", "core.hooksPath=/dev/null"];

fn run_git<const N: usize>(cwd: &Path, args: [&str; N]) -> Result<()> {
    run_git_with_env(cwd, args)
}

fn run_git_with_env<const N: usize>(cwd: &Path, args: [&str; N]) -> Result<()> {
    let status = Command::new("git")
        .args(HOOKS_OFF)
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
    let output = Command::new("git")
        .args(HOOKS_OFF)
        .args(args)
        .current_dir(cwd)
        .output()?;
    anyhow::ensure!(output.status.success(), "shadow git command failed");
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn project_hash(path: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(path.display().to_string().as_bytes());
    crate::hex_lower(&hasher.finalize())
}

/// Best-effort GC of on-disk checkpoint directories older than `retention_days`
/// (#130): removes `checkpoints/<id>/` whose mtime is past the window so the tree
/// can't grow without bound, while keeping recent (still-restorable) checkpoints.
/// Returns the count removed; never fails the caller (a bad entry is skipped).
///
/// F23 (RC-F): each pruned directory's DB row is deleted in the same pass.
/// Storage `gc()` only removes ARCHIVED checkpoint rows, so without this a
/// never-archived old checkpoint would lose its on-disk directory here while its
/// row survived — and a later [`restore_checkpoint`] would then fail on the
/// missing manifest. Deleting the row keeps `checkpoints().list()` and the
/// on-disk directories in agreement. The store is opened once, best-effort: if it
/// can't be opened we still GC the directories.
pub fn gc_old_checkpoint_dirs(retention_days: i64) -> Result<usize> {
    let dir = data_dir()?.join("checkpoints");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(0);
    };
    let cutoff = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(
            retention_days.max(0) as u64 * 86_400,
        ))
        .unwrap_or(std::time::UNIX_EPOCH);
    let store = RuntimeStore::open_default().ok();
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let too_old = entry
            .metadata()
            .and_then(|m| m.modified())
            .map(|mtime| mtime < cutoff)
            .unwrap_or(false);
        if too_old && std::fs::remove_dir_all(&path).is_ok() {
            removed += 1;
            // The directory name IS the checkpoint id — drop the matching DB row
            // so `restore` can't later resolve a row whose manifest is gone.
            if let Some(store) = store.as_ref()
                && let Some(id) = path.file_name().and_then(|name| name.to_str())
                && let Err(error) = store.checkpoints().delete(id)
            {
                tracing::warn!(
                    id,
                    error = %error,
                    "failed to delete DB row for a GC'd checkpoint dir"
                );
            }
        }
    }
    Ok(removed)
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

    #[test]
    fn restore_rejects_paths_escaping_project_root() {
        // Build a real checkpoint, then tamper its on-disk manifest to add
        // entries whose paths escape the project root (one `..`-relative, one
        // absolute), and confirm restore refuses to touch the outside target.
        let pid = std::process::id();
        let root = std::env::temp_dir().join(format!("mermaid_ckpt_escape_{pid}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.txt"), "before").unwrap();

        let manifest = create_checkpoint(&root, &[root.join("a.txt")], None).unwrap();

        // A file OUTSIDE the project root that a tampered manifest tries to delete.
        let outside = std::env::temp_dir().join(format!("mermaid_ckpt_outside_{pid}.txt"));
        std::fs::write(&outside, "do not delete").unwrap();
        let outside_name = outside.file_name().unwrap().to_string_lossy().to_string();

        let manifest_path = data_dir()
            .unwrap()
            .join("checkpoints")
            .join(&manifest.id)
            .join("manifest.json");
        let mut tampered: CheckpointManifest =
            serde_json::from_str(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();
        // existed=false ⇒ restore would try to remove the resolved target.
        tampered.files.push(CheckpointFile {
            path: format!("../{outside_name}"),
            existed: false,
            snapshot_relpath: None,
        });
        tampered.files.push(CheckpointFile {
            path: outside.display().to_string(),
            existed: false,
            snapshot_relpath: None,
        });
        std::fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&tampered).unwrap(),
        )
        .unwrap();

        let _ = restore_checkpoint(&manifest.id).unwrap();

        assert!(
            outside.exists(),
            "restore must not delete a file outside the project root"
        );
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "do not delete");

        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn restore_rejects_tampered_project_root() {
        // #3: a manifest whose `project_path` is rewritten to `/` (so lexical
        // containment passes for ANY absolute path) must be rejected — the
        // root can't be redirected to a filesystem root or disagree with the
        // DB-recorded path.
        let pid = std::process::id();
        let root = std::env::temp_dir().join(format!("mermaid_ckpt_root_{pid}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.txt"), "before").unwrap();
        let manifest = create_checkpoint(&root, &[root.join("a.txt")], None).unwrap();

        let outside = std::env::temp_dir().join(format!("mermaid_ckpt_root_outside_{pid}.txt"));
        std::fs::write(&outside, "do not delete").unwrap();

        let manifest_path = data_dir()
            .unwrap()
            .join("checkpoints")
            .join(&manifest.id)
            .join("manifest.json");
        let mut tampered: CheckpointManifest =
            serde_json::from_str(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();
        tampered.project_path = "/".to_string();
        tampered.files.push(CheckpointFile {
            path: outside.display().to_string(),
            existed: false,
            snapshot_relpath: None,
        });
        std::fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&tampered).unwrap(),
        )
        .unwrap();

        assert!(
            restore_checkpoint(&manifest.id).is_err(),
            "restore must reject a tampered project_path"
        );
        assert!(outside.exists(), "restore must not delete an outside file");
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "do not delete");

        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn mid_restore_failure_restores_nonempty_prior_directory() {
        // F72: a restore that displaces a non-empty directory must put the whole
        // subtree back when a later step fails — not just an empty dir. Drive
        // `apply_restore` to a real mid-way failure (a write whose snapshot source
        // is missing) AFTER a directory has been staged, then assert rollback
        // restored the directory and its contents at every depth.
        use super::{PriorState, RestoreOp, apply_restore, rollback_restore};

        let pid = std::process::id();
        let root = std::env::temp_dir().join(format!("mermaid_ckpt_dirroll_{pid}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        // `victim` is currently a NON-EMPTY directory. The first write replaces
        // this path with a file, which stages the whole subtree aside.
        let victim = root.join("victim");
        std::fs::create_dir_all(victim.join("sub")).unwrap();
        std::fs::write(victim.join("inner.txt"), "precious").unwrap();
        std::fs::write(victim.join("sub").join("deep.txt"), "deep").unwrap();

        // A valid snapshot source for the first (successful) write.
        let src = root.join("snapshot.bin");
        std::fs::write(&src, "new-content").unwrap();

        let staging = root.join(".staging");
        std::fs::create_dir_all(&staging).unwrap();

        let writes = vec![
            RestoreOp::Write {
                target: victim.clone(),
                source: src.clone(),
            },
            // Second write fails: its snapshot source does not exist, so the read
            // errors and the whole restore rolls back.
            RestoreOp::Write {
                target: root.join("other.txt"),
                source: root.join("does-not-exist.bin"),
            },
        ];
        let deletes: Vec<RestoreOp> = Vec::new();

        let mut applied: Vec<PriorState> = Vec::new();
        let result = apply_restore(&writes, &deletes, &staging, &mut applied);
        assert!(
            result.is_err(),
            "a missing snapshot source must fail the restore"
        );

        rollback_restore(&applied);

        // The non-empty directory must be back, contents intact at every depth.
        assert!(victim.is_dir(), "prior directory subtree must be restored");
        assert_eq!(
            std::fs::read_to_string(victim.join("inner.txt")).unwrap(),
            "precious"
        );
        assert_eq!(
            std::fs::read_to_string(victim.join("sub").join("deep.txt")).unwrap(),
            "deep"
        );
        // The failed second write must not have left a file behind.
        assert!(!root.join("other.txt").exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn shadow_git_ignores_absolute_paths_and_cannot_truncate_real_files() {
        // A manifest entry whose `path` stayed ABSOLUTE (a file outside the
        // project root) must never be synced into the shadow worktree:
        // `worktree.join("/abs")` escapes to the real path, and the copy would
        // then `fs::copy(p, p)` — truncating the real file to zero. Guard it.
        let tmp = std::env::temp_dir().join(format!(
            "mermaid_shadow_abs_{}",
            crate::storage::fresh_id("t")
        ));
        let project_root = tmp.join("project");
        std::fs::create_dir_all(&project_root).unwrap();
        let sentinel = tmp.join("outside.txt");
        std::fs::write(&sentinel, "PRECIOUS").unwrap();

        let files = vec![CheckpointFile {
            path: sentinel.display().to_string(), // absolute → must be skipped
            existed: true,
            snapshot_relpath: None,
        }];
        // Best-effort (returns Err if git is unavailable); either way it must
        // never touch the out-of-tree sentinel.
        let _ = super::snapshot_shadow_git(&project_root, &files, "test-cp");
        assert_eq!(
            std::fs::read_to_string(&sentinel).unwrap(),
            "PRECIOUS",
            "shadow-git sync must not truncate a real out-of-tree file",
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
