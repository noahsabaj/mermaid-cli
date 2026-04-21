//! `execute_command` tool.
//!
//! The `ExecContext::token` races the subprocess wait in a `select!`.
//! When the user Ctrl+C's:
//!
//!   1. Reducer emits `Cmd::CancelScope(turn)`.
//!   2. Effect runner cancels the turn's scope token.
//!   3. This tool's select! branch fires, the `Command` is dropped,
//!      `kill_on_drop(true)` reaps the child, and `ToolOutcome::
//!      Cancelled` flows back to the reducer.
//!
//! End-to-end latency: microseconds plus whatever it takes `SIGKILL`
//! to arrive. No polling loop to "forget" to include.
//!
//! The dangerous-command blocklist is defense-in-depth, not a
//! security boundary: the real boundary is the user's decision to
//! run Mermaid with shell access. But the known destructive shapes
//! (`rm -rf /`, fork bombs, dd to device, etc.) are cheap to catch
//! upfront.

use std::process::Stdio;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::constants::{COMMAND_MAX_TIMEOUT_SECS, COMMAND_TIMEOUT_SECS};
use crate::domain::{ToolDefinition, ToolOutcome};

use super::super::ctx::{ExecContext, ProgressEvent};
use super::ToolExecutor;

/// `execute_command` — spawn a shell, run a command, capture output.
///
/// Honors three escape hatches:
/// - `ExecContext::token` (the main event): cancellation from the
///   reducer aborts the child. This is *the* Ctrl+C fix.
/// - `timeout` argument: model-specified per-call cap (capped at
///   `COMMAND_MAX_TIMEOUT_SECS`). Default `COMMAND_TIMEOUT_SECS`.
/// - Dangerous-command blocklist: refuses obvious destructive
///   patterns before spawning.
pub struct ExecuteCommandTool;

#[async_trait]
impl ToolExecutor for ExecuteCommandTool {
    fn name(&self) -> &'static str {
        "execute_command"
    }

    fn schema(&self) -> ToolDefinition {
        ToolDefinition {
            name: "execute_command".to_string(),
            description:
                "Run a shell command. Output is returned after the command completes or times out. Ctrl+C during execution aborts the child immediately."
                    .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Shell command to run." },
                    "working_dir": { "type": "string", "description": "Override working directory (absolute)." },
                    "timeout": {
                        "type": "integer",
                        "description": "Per-call timeout in seconds. Default 30, max 300."
                    }
                },
                "required": ["command"]
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, ctx: ExecContext) -> ToolOutcome {
        let Some(command) = args.get("command").and_then(|v| v.as_str()) else {
            return ToolOutcome::Error {
                error: "execute_command requires 'command' (string)".to_string(),
                duration_secs: 0.0,
            };
        };

        if contains_dangerous_command(command) {
            return ToolOutcome::Error {
                error: format!("Dangerous command blocked: {}", command),
                duration_secs: 0.0,
            };
        }

        let working_dir = args
            .get("working_dir")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let timeout_secs = args
            .get("timeout")
            .and_then(|v| v.as_u64())
            .unwrap_or(COMMAND_TIMEOUT_SECS)
            .min(COMMAND_MAX_TIMEOUT_SECS);

        let command = command.to_string();
        let start = Instant::now();
        let progress = ctx.progress.clone();

        // Spawn + wait. The select! below races three outcomes:
        // subprocess exit, timeout, cancel.
        let mut cmd = Command::new(if cfg!(target_os = "windows") {
            "cmd"
        } else {
            "sh"
        });
        cmd.arg(if cfg!(target_os = "windows") { "/C" } else { "-c" })
            .arg(&command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // `kill_on_drop` on the `Command` — when this
            // Command is dropped (by select! falling through the
            // cancel branch), tokio reaps the child. No orphans.
            .kill_on_drop(true);

        if let Some(dir) = working_dir.as_ref() {
            cmd.current_dir(dir);
        } else {
            cmd.current_dir(&ctx.workdir);
        }

        let run_fut = run_command(cmd, progress);
        let timeout_fut = tokio::time::sleep(Duration::from_secs(timeout_secs));

        tokio::select! {
            biased;
            _ = ctx.token.cancelled() => ToolOutcome::Cancelled,
            _ = timeout_fut => ToolOutcome::Finished {
                output: format!(
                    "Command timed out after {} seconds. The process is likely still running in the background. \
                     This is normal for GUI apps, servers, and long-running processes.",
                    timeout_secs
                ),
                images: None,
                duration_secs: start.elapsed().as_secs_f64(),
            },
            result = run_fut => match result {
                Ok(output) => ToolOutcome::Finished {
                    output,
                    images: None,
                    duration_secs: start.elapsed().as_secs_f64(),
                },
                Err(e) => ToolOutcome::Error {
                    error: format!("Command failed: {}", e),
                    duration_secs: start.elapsed().as_secs_f64(),
                },
            },
        }
    }
}

/// Drive the child process, pumping stdout+stderr concurrently so
/// the kernel pipe buffer never wedges the child. Emits
/// `ProgressEvent::Output` chunks on `ExecContext::progress` for
/// any future consumer that wants to show live subprocess output.
async fn run_command(
    mut cmd: Command,
    progress: tokio::sync::mpsc::Sender<ProgressEvent>,
) -> std::io::Result<String> {
    let mut child = cmd.spawn()?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("child stdout unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| std::io::Error::other("child stderr unavailable"))?;

    let progress_clone = progress.clone();
    let stdout_task = tokio::spawn(async move {
        let mut reader = BufReader::new(stdout).lines();
        let mut output = String::new();
        while let Ok(Some(line)) = reader.next_line().await {
            let _ = progress_clone
                .send(ProgressEvent::Output(line.clone()))
                .await;
            output.push_str(&line);
            output.push('\n');
        }
        output
    });

    let stderr_task = tokio::spawn(async move {
        let mut reader = BufReader::new(stderr).lines();
        let mut errors = String::new();
        while let Ok(Some(line)) = reader.next_line().await {
            errors.push_str(&line);
            errors.push('\n');
        }
        errors
    });

    let output = stdout_task.await.unwrap_or_default();
    let errors = stderr_task.await.unwrap_or_default();
    let status = child.wait().await?;

    let mut full_output = output;
    if !errors.is_empty() {
        full_output.push_str("\n--- stderr ---\n");
        full_output.push_str(&errors);
    }
    if !status.success() {
        full_output.push_str(&format!(
            "\n--- Command exited with status: {} ---",
            status.code().unwrap_or(-1)
        ));
    }
    Ok(full_output)
}

/// Defense-in-depth check for obviously destructive commands. Same
/// Applies the same patterns historically shipped with Mermaid
/// — documented there as a blocklist, NOT a security boundary. The
/// real boundary is the user's decision to grant shell access to the
/// AI.
///
/// Known residual gaps (documented for honesty, not as bugs):
/// - Encoded payloads (`echo ... | base64 -d | sh`).
/// - `eval` / `exec` chains where literal `rm` never appears.
/// - Script languages (`python -c ...`, `node -e ...`).
/// - Nested expansions beyond `$(...)` and backticks.
fn contains_dangerous_command(command: &str) -> bool {
    let dangerous_patterns = [
        "rm -rf /",
        "rm -rf /*",
        "dd if=/dev/zero of=/",
        "dd if=/dev/random of=/",
        "dd if=/dev/urandom of=/",
        "mkfs.",
        "format c:",
        "> /dev/sda",
        "chmod -R 777 /",
        "chmod -R 000 /",
        ":(){ :|:& };:",
        ":(){ :|:&};:",
        "curl | bash",
        "curl | sh",
        "wget | bash",
        "wget | sh",
        "nc -l",
        "ncat -l",
        "socat tcp-listen:",
    ];

    let lower = command.to_lowercase();
    for pattern in &dangerous_patterns {
        if lower.contains(pattern) {
            return true;
        }
    }

    let system_dir_patterns: [(&str, bool); 10] = [
        ("/etc", false),
        ("/usr", false),
        ("/boot", false),
        ("/proc", false),
        ("/sys", false),
        ("/dev/", true),
        ("/home", false),
        ("C:\\Windows", false),
        ("C:\\Program Files", false),
        ("C:\\Users", false),
    ];

    let has_rm = lower.starts_with("rm ")
        || lower.contains(" rm ")
        || lower.contains(";rm ")
        || lower.contains("&rm ")
        || lower.contains("|rm ")
        || lower.contains("$(rm ")
        || lower.contains("`rm ");
    let has_del = lower.starts_with("del ")
        || lower.contains(" del ")
        || lower.contains(";del ")
        || lower.contains("&del ")
        || lower.contains("$(del ")
        || lower.contains("`del ");

    if has_rm || has_del {
        for (dir, require_trailing) in &system_dir_patterns {
            if *require_trailing {
                if command.contains(dir)
                    && !command.contains(&format!("{}null", dir))
                    && !command.contains(&format!("{}zero", dir))
                {
                    return true;
                }
            } else if command.contains(dir) {
                return true;
            }
        }
        if command.contains(" ~/")
            || command.ends_with(" ~")
            || command.contains(" ~ ")
            || command.contains("$HOME")
        {
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ToolCallId, TurnId};
    use crate::providers::ctx::test_exec_context;
    use std::path::PathBuf;

    #[tokio::test]
    async fn safe_command_runs_and_captures_output() {
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), PathBuf::from("/tmp"));
        let outcome = ExecuteCommandTool
            .execute(serde_json::json!({"command": "echo hello world"}), ctx)
            .await;
        match outcome {
            ToolOutcome::Finished { output, .. } => assert!(output.contains("hello world")),
            other => panic!("expected Finished: {:?}", other),
        }
    }

    #[tokio::test]
    async fn dangerous_command_blocked() {
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), PathBuf::from("/tmp"));
        let outcome = ExecuteCommandTool
            .execute(serde_json::json!({"command": "rm -rf /"}), ctx)
            .await;
        match outcome {
            ToolOutcome::Error { error, .. } => assert!(error.contains("Dangerous")),
            other => panic!("expected Error: {:?}", other),
        }
    }

    #[tokio::test]
    async fn cancellation_aborts_long_running_command() {
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), PathBuf::from("/tmp"));
        let token = ctx.token.clone();
        let handle = tokio::spawn(async move {
            ExecuteCommandTool
                .execute(serde_json::json!({"command": "sleep 10"}), ctx)
                .await
        });
        // Give the child a beat to spawn, then cancel.
        tokio::time::sleep(Duration::from_millis(30)).await;
        token.cancel();
        let start = Instant::now();
        let outcome = tokio::time::timeout(Duration::from_millis(500), handle)
            .await
            .expect("didn't hang")
            .expect("join");
        let elapsed = start.elapsed();
        assert!(matches!(outcome, ToolOutcome::Cancelled));
        assert!(
            elapsed < Duration::from_millis(200),
            "cancellation took {:?}",
            elapsed
        );
    }

    #[tokio::test]
    async fn timeout_honored() {
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), PathBuf::from("/tmp"));
        let outcome = ExecuteCommandTool
            .execute(serde_json::json!({"command": "sleep 5", "timeout": 1}), ctx)
            .await;
        match outcome {
            ToolOutcome::Finished { output, .. } => {
                assert!(output.contains("timed out"));
            },
            other => panic!("expected timeout Finished: {:?}", other),
        }
    }

    #[test]
    fn dangerous_detection_covers_known_shapes() {
        assert!(contains_dangerous_command("rm -rf /"));
        assert!(contains_dangerous_command(":(){ :|:& };:"));
        assert!(contains_dangerous_command("ncat -l 8080"));
        assert!(!contains_dangerous_command("ls -la"));
        assert!(!contains_dangerous_command("cargo build"));
        assert!(!contains_dangerous_command(
            r#"find . -type f ! -path "./.git/*" ! -path "./.mermaid/*" 2>/dev/null"#
        ));
    }
}
