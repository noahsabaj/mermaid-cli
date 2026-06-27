use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::pathguard::contain_within;
use crate::{ApprovalRecord, RuntimeStore};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalReplayResult {
    pub approval: Option<ApprovalRecord>,
    pub replayed: bool,
    pub summary: String,
}

pub fn approve_and_replay(id: &str) -> Result<ApprovalReplayResult> {
    let store = RuntimeStore::open_default()?;
    store.approvals().decide(id, "approved")?;
    let approval = store
        .approvals()
        .get(id)?
        .with_context(|| format!("approval not found after approval: {id}"))?;

    let Some(raw_action) = approval.pending_action_json.as_deref() else {
        return Ok(ApprovalReplayResult {
            approval: Some(approval),
            replayed: false,
            summary: "approval recorded; no pending action was stored".to_string(),
        });
    };

    let action: serde_json::Value = serde_json::from_str(raw_action)
        .with_context(|| format!("approval {id} pending action was not valid JSON"))?;
    let summary = replay_pending_action(&action)?;
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

    match tool {
        "execute_command" => replay_execute_command(args, &workdir),
        "write_file" => {
            let path = string_arg(args, "path")?;
            let content = string_arg(args, "content")?;
            let target = contain_within(&workdir, path)?;
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&target, content)?;
            Ok(format!("replayed write_file {}", target.display()))
        },
        "edit_file" => {
            let path = string_arg(args, "path")?;
            let old = string_arg(args, "old_string")?;
            let new = string_arg(args, "new_string")?;
            let target = contain_within(&workdir, path)?;
            replay_edit(&target, old, new)?;
            Ok(format!("replayed edit_file {}", target.display()))
        },
        "delete_file" => {
            let path = string_arg(args, "path")?;
            let target = contain_within(&workdir, path)?;
            std::fs::remove_file(&target)?;
            Ok(format!("replayed delete_file {}", target.display()))
        },
        "create_directory" => {
            let path = string_arg(args, "path")?;
            let target = contain_within(&workdir, path)?;
            std::fs::create_dir_all(&target)?;
            Ok(format!("replayed create_directory {}", target.display()))
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
    // Confine the replay cwd to the recorded workdir. The command itself is an
    // already-approved shell string (replayed verbatim), but a tampered
    // `working_dir` must not let the replay escape the project root — this
    // mirrors the containment the write/edit/delete arms already apply.
    let effective_dir = match args.get("working_dir").and_then(|value| value.as_str()) {
        Some(dir) => contain_within(workdir, dir)?,
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

fn replay_edit(path: &Path, old_string: &str, new_string: &str) -> Result<()> {
    let current = std::fs::read_to_string(path)?;
    let count = current.matches(old_string).count();
    anyhow::ensure!(count > 0, "old_string not found during approval replay");
    anyhow::ensure!(
        count == 1,
        "old_string appears {count} times during approval replay"
    );
    std::fs::write(path, current.replacen(old_string, new_string, 1))?;
    Ok(())
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
        assert!(contain_within(&root, "../escape").is_err());
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
}
