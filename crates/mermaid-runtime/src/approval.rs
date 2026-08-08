use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::{ApprovalRecord, RuntimeStore};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalReplayResult {
    pub approval: Option<ApprovalRecord>,
    pub replayed: bool,
    pub summary: String,
}

/// Approve pending action `id`, replay its effect, then mark it approved.
///
/// # Errors
///
/// Opening the runtime store, no approval with that `id`, an approval already
/// decided or archived, losing the atomic claim to a concurrent `approve`, and
/// any failure while replaying the action itself. On every failure after the
/// claim, the claim is released and the approval stays undecided — a caller
/// seeing `Err` may retry, and must not assume the effect did not run: a
/// replay that half-applied and then failed reports `Err` too.
pub fn approve_and_replay(id: &str) -> Result<ApprovalReplayResult> {
    let store = RuntimeStore::open_default()?;
    approve_and_replay_with(&store, id)
}

/// Core of [`approve_and_replay`] with the store injected (so it is unit-testable
/// against a temp DB).
///
/// `replay_pending_action` performs a filesystem write or a process spawn —
/// effects SQLite cannot roll back — so they run *before* the "approved" mark is
/// written. A crash mid-replay therefore leaves the approval undecided and
/// safely re-runnable, never "approved but never applied" (#62). The single-shot
/// `decide` is the last mutation; its `WHERE user_decision IS NULL` guard closes
/// the residual same-user race and makes a second call a no-op error.
pub(crate) fn approve_and_replay_with(
    store: &RuntimeStore,
    id: &str,
) -> Result<ApprovalReplayResult> {
    // Load *without* deciding, and refuse anything already decided or archived
    // (mirrors `decide`'s `WHERE`), so a denied/archived approval can't be replayed.
    let approval = store
        .approvals()
        .get(id)?
        .with_context(|| format!("approval not found: {id}"))?;
    anyhow::ensure!(
        approval.user_decision.is_none() && approval.archived_at.is_none(),
        "approval {id} cannot be replayed (already decided or archived)"
    );

    // Claim atomically so two concurrent `approve <id>` calls can't both run the
    // un-rollback-able effect (#118): exactly one wins the claim and proceeds.
    // Any failure before the final mark releases the claim, keeping the action
    // re-runnable — preserving the effect-before-mark crash-safety of #62.
    anyhow::ensure!(
        store.approvals().claim(id)?,
        "approval {id} is already being applied by a concurrent approve"
    );
    match replay_after_claim(store, id, &approval) {
        Ok(result) => Ok(result),
        Err(error) => {
            let _ = store.approvals().release_claim(id);
            Err(error)
        },
    }
}

/// Run a claimed approval's pending action and finalize it. The caller holds the
/// `approving` claim and releases it if this returns `Err`.
fn replay_after_claim(
    store: &RuntimeStore,
    id: &str,
    approval: &ApprovalRecord,
) -> Result<ApprovalReplayResult> {
    let Some(raw_action) = approval.pending_action_json.as_deref() else {
        // Nothing to replay: just record the decision (no effect to order around).
        store.approvals().finalize_claimed(id, "approved")?;
        let approval = store.approvals().get(id)?;
        return Ok(ApprovalReplayResult {
            approval,
            replayed: false,
            summary: "approval recorded; no pending action was stored".to_string(),
        });
    };

    let action: serde_json::Value = serde_json::from_str(raw_action)
        .with_context(|| format!("approval {id} pending action was not valid JSON"))?;
    // 1) Effect first — the un-rollback-able fs write / process spawn.
    let summary = replay_pending_action(&action)?;
    // 2) Mark approved last — only now is "approved" both true and durable.
    store.approvals().finalize_claimed(id, "approved")?;
    let approval = store
        .approvals()
        .get(id)?
        .with_context(|| format!("approval not found after approval: {id}"))?;

    // 3) Best-effort bookkeeping.
    let _ = crate::run_plugin_hooks(
        "approval_decided",
        &serde_json::json!({
            "id": approval.id.clone(),
            "decision": "approved",
            "task_id": approval.task_id.clone(),
            "replayed": true,
            "summary": summary.clone(),
        }),
    );
    if let Some(task_id) = approval.task_id.as_deref() {
        let _ = store
            .tasks()
            .add_event(task_id, "approval_replayed", &summary);
    }
    if let Some(tool) = action.get("tool").and_then(|value| value.as_str()) {
        let replay_run = store.tool_runs().start(crate::NewToolRun {
            id: None,
            task_id: approval.task_id.clone(),
            turn_id: action
                .get("turn_id")
                .and_then(|value| value.as_i64())
                .map(|value| value.to_string()),
            call_id: action
                .get("call_id")
                .and_then(|value| value.as_i64())
                .map(|value| value.to_string()),
            tool_name: format!("approval_replay:{tool}"),
            args_json: Some(raw_action.to_string()),
        });
        if let Ok(run) = replay_run {
            let _ = store.tool_runs().finish(
                &run.id,
                "success",
                Some(&serde_json::json!({"summary": summary}).to_string()),
            );
        }
    }
    Ok(ApprovalReplayResult {
        approval: Some(approval),
        replayed: true,
        summary,
    })
}

/// Mark pending action `id` denied. Nothing is replayed.
///
/// # Errors
///
/// Opening the runtime store, and the `decide` write itself — which rejects an
/// `id` that does not exist or was already decided. The plugin hook and the
/// task event afterwards are best-effort and cannot fail the call, so an `Ok`
/// means the denial is recorded, not that either of those was delivered.
pub fn deny_approval(id: &str) -> Result<ApprovalReplayResult> {
    let store = RuntimeStore::open_default()?;
    store.approvals().decide(id, "denied")?;
    let approval = store.approvals().get(id)?;
    let _ = crate::run_plugin_hooks(
        "approval_decided",
        &serde_json::json!({
            "id": id,
            "decision": "denied",
            "task_id": approval.as_ref().and_then(|record| record.task_id.clone()),
            "replayed": false,
        }),
    );
    if let Some(task_id) = approval
        .as_ref()
        .and_then(|record| record.task_id.as_deref())
    {
        let _ =
            store
                .tasks()
                .add_event(task_id, "approval_denied", &format!("approval {id} denied"));
    }
    Ok(ApprovalReplayResult {
        approval,
        replayed: false,
        summary: "approval denied".to_string(),
    })
}

fn replay_pending_action(action: &serde_json::Value) -> Result<String> {
    let tool = action
        .get("tool")
        .and_then(|value| value.as_str())
        .context("pending action missing string `tool`")?;
    let workdir = action
        .get("workdir")
        .and_then(|value| value.as_str())
        .map(PathBuf::from)
        .unwrap_or(std::env::current_dir()?);
    let args = action.get("args").unwrap_or(action);

    // The stored path comes from (potentially tampered) replay state, so confine
    // every effect with the symlink-safe `*_beneath` helpers — the SAME
    // kernel-confined (`openat2(RESOLVE_BENEATH)`) path the live filesystem tools
    // use — rather than by-path `std::fs`, which follows an in-repo symlink out
    // of the root (#F4/#F5). `relative_within` rejects an escaping path and names
    // it relative to `workdir`, which the helpers resolve beneath a `workdir` fd.
    match tool {
        "execute_command" => replay_execute_command(args, &workdir),
        "write_file" => {
            let path = string_arg(args, "path")?;
            let content = string_arg(args, "content")?;
            let rel = crate::pathguard::relative_within(&workdir, path)?;
            if let Some(parent) = rel.parent()
                && !parent.as_os_str().is_empty()
            {
                crate::pathguard::create_dir_all_beneath(&workdir, parent)?;
            }
            crate::pathguard::write_atomic_beneath(&workdir, &rel, content.as_bytes())?;
            Ok(format!(
                "replayed write_file {}",
                workdir.join(&rel).display()
            ))
        },
        "apply_patch" => {
            let patch = string_arg(args, "patch")?;
            let hunks = crate::apply_patch::parse_patch(patch)
                .map_err(|e| anyhow::anyhow!("apply_patch replay: {e}"))?;
            for hunk in &hunks {
                replay_apply_hunk(&workdir, hunk)?;
            }
            Ok(format!("replayed apply_patch ({} hunk(s))", hunks.len()))
        },
        "delete_file" => {
            let path = string_arg(args, "path")?;
            let rel = crate::pathguard::relative_within(&workdir, path)?;
            crate::pathguard::remove_file_beneath(&workdir, &rel)?;
            Ok(format!(
                "replayed delete_file {}",
                workdir.join(&rel).display()
            ))
        },
        "create_directory" => {
            let path = string_arg(args, "path")?;
            let rel = crate::pathguard::relative_within(&workdir, path)?;
            crate::pathguard::create_dir_all_beneath(&workdir, &rel)?;
            Ok(format!(
                "replayed create_directory {}",
                workdir.join(&rel).display()
            ))
        },
        other => anyhow::bail!("approval replay does not support tool `{other}`"),
    }
}

/// Provider keys + name patterns that must not leak into a replayed child's
/// environment. Mirrors `providers::tool::exec::is_secret_env_name` in the main
/// crate (the runtime crate can't depend on it). Denylist, so ordinary
/// build/run vars (`PATH`, toolchain, `XAUTHORITY`, …) survive.
fn is_secret_env_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    upper.contains("API_KEY")
        || upper.contains("APIKEY")
        || upper.contains("ACCESS_KEY")
        || upper.contains("PRIVATE_KEY")
        || upper.contains("SECRET")
        || upper.contains("PASSWORD")
        || upper.contains("PASSWD")
        || upper.contains("TOKEN")
        || upper.contains("CREDENTIAL")
}

/// Strip secret-bearing env vars from a replay child. The daemon's environment
/// holds provider API keys and the pairing token; an approved shell command
/// must not be able to read them back out (#24).
fn scrub_secret_env(cmd: &mut Command) {
    for (name, _) in std::env::vars() {
        if is_secret_env_name(&name) {
            cmd.env_remove(&name);
        }
    }
}

fn replay_execute_command(args: &serde_json::Value, workdir: &Path) -> Result<String> {
    let command = string_arg(args, "command")?;
    // Re-apply the destructive hard-deny on replay. The command was approved by a
    // human originally, but `pending_action_json` is stored, tamperable state; a
    // doctored record must not turn `approve` into arbitrary destructive exec
    // (#F7). Destructive shapes are hard-denied (never merely "ask"), so a
    // legitimate pending approval can't be destructive in the first place.
    anyhow::ensure!(
        !crate::policy::is_destructive_command(command),
        "approval replay refused: the stored command is classified destructive \
         (possible tampered approval record)"
    );
    // Confine the replay cwd to the recorded workdir. The command itself is an
    // already-approved shell string (replayed verbatim), but a tampered
    // `working_dir` must not let the replay escape the project root. Use the
    // canonical (symlink-resolving) check so a symlinked working_dir whose real
    // target is outside the root is rejected, not just a lexical `..` (#F6).
    let effective_dir = match args.get("working_dir").and_then(|value| value.as_str()) {
        Some(dir) => crate::pathguard::contain_within_canonical(workdir, dir)?,
        None => workdir.to_path_buf(),
    };
    let mode = args
        .get("mode")
        .and_then(|value| value.as_str())
        .unwrap_or("wait");
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(command).current_dir(&effective_dir);
    scrub_secret_env(&mut cmd);
    if mode == "background" {
        // New process group so the detached child (and anything it forks) can be
        // signalled/reaped as a unit rather than leaking grandchildren (#24).
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let child = cmd
            .spawn()
            .with_context(|| format!("failed to replay background command `{command}`"))?;
        return Ok(format!(
            "replayed execute_command in background with pid {}",
            child.id()
        ));
    }
    let output = cmd
        .output()
        .with_context(|| format!("failed to replay command `{command}`"))?;
    anyhow::ensure!(
        output.status.success(),
        "replayed command failed with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(format!(
        "replayed execute_command successfully ({} stdout bytes)",
        output.stdout.len()
    ))
}

/// Re-apply one parsed patch hunk beneath `root` for approval replay, using the
/// SAME confined pathguard helpers as the live tool so a symlinked leaf inside
/// the root can neither leak an outside file nor redirect a write (#F5).
fn replay_apply_hunk(root: &Path, hunk: &crate::apply_patch::Hunk) -> Result<()> {
    use crate::apply_patch::Hunk;
    match hunk {
        Hunk::AddFile { path, contents } => {
            let rel = crate::pathguard::relative_within(root, &path.to_string_lossy())?;
            replay_ensure_parent(root, &rel)?;
            crate::pathguard::write_atomic_beneath(root, &rel, contents.as_bytes())?;
        },
        Hunk::DeleteFile { path } => {
            let rel = crate::pathguard::relative_within(root, &path.to_string_lossy())?;
            crate::pathguard::remove_file_beneath(root, &rel)?;
        },
        Hunk::UpdateFile {
            path,
            move_path,
            chunks,
        } => {
            let src_rel = crate::pathguard::relative_within(root, &path.to_string_lossy())?;
            let original = replay_read_beneath(root, &src_rel)?;
            let applied = crate::apply_patch::derive_new_contents(&original, chunks)
                .map_err(|e| anyhow::anyhow!("apply_patch replay for {}: {e}", path.display()))?;
            let dst_rel = match move_path {
                Some(mv) => crate::pathguard::relative_within(root, &mv.to_string_lossy())?,
                None => src_rel.clone(),
            };
            replay_ensure_parent(root, &dst_rel)?;
            crate::pathguard::write_atomic_beneath(
                root,
                &dst_rel,
                applied.new_contents.as_bytes(),
            )?;
            if src_rel != dst_rel {
                crate::pathguard::remove_file_beneath(root, &src_rel)?;
            }
        },
    }
    Ok(())
}

fn replay_ensure_parent(root: &Path, rel: &Path) -> Result<()> {
    if let Some(parent) = rel.parent()
        && !parent.as_os_str().is_empty()
    {
        crate::pathguard::create_dir_all_beneath(root, parent)?;
    }
    Ok(())
}

fn replay_read_beneath(root: &Path, rel: &Path) -> Result<String> {
    use std::io::Read;
    let mut file = crate::pathguard::open_beneath(root, rel, crate::pathguard::OpenIntent::Read)?;
    let mut s = String::new();
    file.read_to_string(&mut s)?;
    Ok(s)
}

fn string_arg<'a>(args: &'a serde_json::Value, name: &str) -> Result<&'a str> {
    args.get(name)
        .and_then(|value| value.as_str())
        .with_context(|| format!("pending action missing string arg `{name}`"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_path_rejects_parent_escape() {
        let root = std::env::temp_dir().join("mermaid_replay_root");
        assert!(crate::pathguard::relative_within(&root, "../escape").is_err());
    }

    /// RC-B: a write replayed through an in-repo symlink that escapes the root
    /// must be refused, and nothing may be written outside. The live tool path
    /// already had this guarantee via the `*_beneath` helpers; the replay path
    /// now shares it instead of following the symlink with by-path `std::fs`.
    #[cfg(target_os = "linux")]
    #[test]
    fn replay_write_refuses_escaping_symlink() {
        let root =
            std::env::temp_dir().join(format!("mermaid_replay_symlink_{}", std::process::id()));
        let outside =
            std::env::temp_dir().join(format!("mermaid_replay_outside_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        // Plant a symlink inside the root that redirects outside it.
        std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();

        let action = serde_json::json!({
            "tool": "write_file",
            "workdir": root,
            "args": {"path": "escape/evil.txt", "content": "pwned"}
        });
        assert!(
            replay_pending_action(&action).is_err(),
            "a write through an escaping symlink must be refused"
        );
        assert!(
            !outside.join("evil.txt").exists(),
            "nothing may be written outside the root"
        );
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// RC-B/#F7: a tampered pending record whose command is destructive must be
    /// refused on replay rather than executed verbatim.
    #[test]
    fn replay_execute_command_refuses_destructive() {
        let root =
            std::env::temp_dir().join(format!("mermaid_replay_destr_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let action = serde_json::json!({
            "tool": "execute_command",
            "workdir": root,
            "args": {"command": "rm -rf /"}
        });
        assert!(
            replay_pending_action(&action).is_err(),
            "a destructive stored command must be refused on replay"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn replay_execute_command_rejects_escaping_working_dir() {
        let root = std::env::temp_dir().join(format!("mermaid_replay_exec_{}", std::process::id()));
        let action = serde_json::json!({
            "tool": "execute_command",
            "workdir": root,
            "args": {"command": "true", "working_dir": "../escape"}
        });
        // Containment fails before any shell is spawned, so this is portable.
        assert!(
            replay_pending_action(&action).is_err(),
            "an escaping working_dir must be rejected before exec"
        );
    }

    #[test]
    fn replay_write_file_creates_parent() {
        let root =
            std::env::temp_dir().join(format!("mermaid_replay_write_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let action = serde_json::json!({
            "tool": "write_file",
            "workdir": root,
            "args": {"path": "a/b.txt", "content": "ok"}
        });
        let summary = replay_pending_action(&action).unwrap();
        assert!(summary.contains("write_file"));
    }

    fn temp_store(name: &str) -> RuntimeStore {
        let dir = std::env::temp_dir().join(format!("mermaid_approval_{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("runtime.sqlite3");
        RuntimeStore::open(&path).expect("open store")
    }

    fn pending_approval(store: &RuntimeStore, action: &serde_json::Value) -> String {
        store
            .approvals()
            .create(crate::NewApproval {
                task_id: None,
                proposed_action: "test".to_string(),
                risk_classification: "low".to_string(),
                policy_decision: "ask".to_string(),
                args_summary: None,
                checkpoint_id: None,
                pending_action_json: Some(action.to_string()),
            })
            .expect("create approval")
            .id
    }

    fn temp_workdir(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("mermaid_approve_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn approve_replay_marks_approved_only_after_effect() {
        let store = temp_store("ok");
        let root = temp_workdir("ok");
        let action = serde_json::json!({
            "tool": "write_file",
            "workdir": root,
            "args": {"path": "out.txt", "content": "hello"}
        });
        let id = pending_approval(&store, &action);

        let result = approve_and_replay_with(&store, &id).expect("replay should succeed");
        assert!(result.replayed);
        assert!(
            root.join("out.txt").exists(),
            "the file effect must have run"
        );
        let decided = store.approvals().get(&id).unwrap().unwrap();
        assert_eq!(decided.user_decision.as_deref(), Some("approved"));
    }

    #[test]
    fn failed_replay_leaves_approval_pending() {
        let store = temp_store("fail");
        let root = temp_workdir("fail");
        // delete_file of a path that doesn't exist → replay errors *after* the
        // pending pre-check but *before* `decide`, so the approval must stay
        // undecided (re-runnable), never "approved but never applied" (#62).
        let action = serde_json::json!({
            "tool": "delete_file",
            "workdir": root,
            "args": {"path": "nope.txt"}
        });
        let id = pending_approval(&store, &action);

        assert!(approve_and_replay_with(&store, &id).is_err());
        let still = store.approvals().get(&id).unwrap().unwrap();
        assert!(
            still.user_decision.is_none(),
            "a failed replay must not mark the approval approved"
        );
    }

    #[test]
    fn second_approve_is_single_shot() {
        let store = temp_store("twice");
        let root = temp_workdir("twice");
        let action = serde_json::json!({
            "tool": "create_directory",
            "workdir": root,
            "args": {"path": "sub"}
        });
        let id = pending_approval(&store, &action);

        approve_and_replay_with(&store, &id).expect("first approve succeeds");
        assert!(
            approve_and_replay_with(&store, &id).is_err(),
            "a second approve must be rejected (already decided)"
        );
        let decided = store.approvals().get(&id).unwrap().unwrap();
        assert_eq!(decided.user_decision.as_deref(), Some("approved"));
    }
}
