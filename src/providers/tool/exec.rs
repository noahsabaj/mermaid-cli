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

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::constants::{COMMAND_MAX_TIMEOUT_SECS, COMMAND_TIMEOUT_SECS};
use crate::domain::{
    ManagedProcess, ManagedProcessStatus, ToolDefinition, ToolMetadata, ToolOutcome,
    ToolRunMetadata,
};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandMode {
    Wait,
    Background,
}

impl CommandMode {
    fn parse(args: &serde_json::Value) -> Result<Self, String> {
        match args.get("mode").and_then(|v| v.as_str()).unwrap_or("wait") {
            "wait" | "foreground" => Ok(Self::Wait),
            "background" => Ok(Self::Background),
            other => Err(format!(
                "execute_command: mode must be 'wait' or 'background', got '{}'",
                other
            )),
        }
    }
}

#[async_trait]
impl ToolExecutor for ExecuteCommandTool {
    fn name(&self) -> &'static str {
        "execute_command"
    }

    fn schema(&self) -> ToolDefinition {
        ToolDefinition {
            name: "execute_command".to_string(),
            description:
                "Run a shell command. Use mode='wait' for finite commands, or mode='background' for dev servers and GUI/daemon-style commands that should keep running after the tool returns. Ctrl+C during foreground execution aborts the child immediately."
                    .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Shell command to run." },
                    "working_dir": { "type": "string", "description": "Override working directory (absolute)." },
                    "mode": {
                        "type": "string",
                        "enum": ["wait", "background"],
                        "default": "wait",
                        "description": "Use 'background' for long-running servers, daemons, and GUI launchers."
                    },
                    "timeout": {
                        "type": "integer",
                        "description": "Per-call foreground timeout in seconds. Default 30, max 300. Foreground timeout kills the child."
                    },
                    "startup_timeout_secs": {
                        "type": "integer",
                        "description": "Background mode: seconds to watch startup logs for readiness. Default 5, max 30."
                    },
                    "ready_pattern": {
                        "type": "string",
                        "description": "Background mode: text that marks the server/app ready when it appears in the startup log."
                    },
                    "open_url": {
                        "type": "string",
                        "description": "Background mode: URL to open with the default browser after startup."
                    }
                },
                "required": ["command"]
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, ctx: ExecContext) -> ToolOutcome {
        let Some(command) = args.get("command").and_then(|v| v.as_str()) else {
            return ToolOutcome::error("execute_command requires 'command' (string)", 0.0);
        };

        if contains_dangerous_command(command) {
            return ToolOutcome::error(format!("Dangerous command blocked: {}", command), 0.0);
        }

        // Resolve the effective working directory and decide containment. An
        // out-of-project cwd is allowed but escalated to ExternalDirectory so
        // the gate won't auto-allow even a read-only command run outside the
        // project — closing the working_dir containment bypass.
        let (effective_workdir, within_project) = match args
            .get("working_dir")
            .and_then(|v| v.as_str())
        {
            Some(raw) => match super::path_safety::resolve_path_within(&ctx.workdir, raw) {
                Ok(resolved) => resolved,
                Err(e) => {
                    return ToolOutcome::error(format!("execute_command working_dir: {e}"), 0.0);
                },
            },
            None => (ctx.workdir.clone(), true),
        };

        let category = if within_project {
            crate::runtime::ToolCategory::Shell
        } else {
            crate::runtime::ToolCategory::ExternalDirectory
        };
        let mut policy_request =
            crate::runtime::ActionRequest::new("execute_command", category, command.to_string());
        policy_request.command = Some(command.to_string());
        if !within_project {
            policy_request.path = Some(effective_workdir.display().to_string());
        }
        let pending_action = serde_json::json!({
            "tool": "execute_command",
            "args": args.clone(),
            "workdir": effective_workdir.display().to_string(),
            "turn_id": ctx.turn.0,
            "call_id": ctx.call_id.0,
            "task_id": ctx.task_id.clone(),
        });
        // Central safety gate. An Ask decision is handled inside the gate
        // (checkpoint + approval row + blocking outcome). Allow returns the
        // classified risk so we can take the pre-existing Allow-path
        // checkpoint below.
        match super::policy_gate::gate(&ctx, policy_request, &[], pending_action.clone(), true)
            .await
        {
            super::policy_gate::Gate::Block(outcome) => return outcome,
            super::policy_gate::Gate::Proceed { risk } => {
                if ctx.config.safety.checkpoint_on_mutation
                    && risk != crate::runtime::RiskClass::ReadOnly
                {
                    let _ = crate::runtime::create_checkpoint_for_task(
                        &ctx.workdir,
                        &[],
                        Some(pending_action.clone()),
                        ctx.task_id.clone(),
                    );
                }
            },
        }

        let mode = match CommandMode::parse(&args) {
            Ok(mode) => mode,
            Err(error) => return ToolOutcome::error(error, 0.0),
        };
        let shell_payload = serde_json::json!({
            "task_id": ctx.task_id.clone(),
            "turn_id": ctx.turn.0,
            "call_id": ctx.call_id.0,
            "command": command,
            "working_dir": effective_workdir.display().to_string(),
        });
        let _ = crate::runtime::run_plugin_hooks("before_shell", &shell_payload);
        if mode == CommandMode::Background {
            let startup_timeout_secs = args
                .get("startup_timeout_secs")
                .or_else(|| args.get("startup_timeout"))
                .and_then(|v| v.as_u64())
                .unwrap_or(5)
                .clamp(1, 30);
            let ready_pattern = args
                .get("ready_pattern")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let open_url = args
                .get("open_url")
                .and_then(|v| v.as_str())
                .filter(|v| !v.trim().is_empty())
                .map(str::to_string);
            let outcome = run_background_command(
                command,
                &effective_workdir,
                startup_timeout_secs,
                ready_pattern.as_deref(),
                open_url.as_deref(),
                ctx,
            )
            .await;
            let _ = crate::runtime::run_plugin_hooks(
                "after_shell",
                &serde_json::json!({
                    "command": command,
                    "status": format!("{:?}", outcome.status),
                    "summary": &outcome.summary,
                }),
            );
            return outcome;
        }

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
            // `kill_on_drop` reaps the direct shell child; the cancel branch in
            // `run_command` additionally tree-kills the process group, so a
            // forked grandchild (e.g. `sh -c "server &"`) isn't orphaned.
            .kill_on_drop(true);

        // Unix: lead a new process group so cancel can signal the whole group.
        // (Windows kills the tree by pid via `taskkill /T`, no group needed.)
        #[cfg(unix)]
        cmd.process_group(0);

        cmd.current_dir(&effective_workdir);
        scrub_secret_env(&mut cmd);

        let run_fut = run_command(cmd, progress, ctx.token.clone(), ctx.background.clone());
        let timeout_fut = tokio::time::sleep(Duration::from_secs(timeout_secs));

        let outcome = tokio::select! {
            biased;
            _ = timeout_fut => {
                let message = format!(
                    "Command timed out after {} seconds and was killed. \
                     For dev servers, GUI apps, or other long-running commands, call execute_command with mode=\"background\".",
                    timeout_secs
                );
                let duration_secs = start.elapsed().as_secs_f64();
                ToolOutcome::error(message, duration_secs).with_metadata(command_metadata(
                    CommandMetadataInput {
                        command: command.clone(),
                        working_dir: Some(effective_workdir.display().to_string()),
                        exit_code: None,
                        timed_out: true,
                        background: false,
                        stdout_lines: 0,
                        stderr_lines: 0,
                        detected_urls: Vec::new(),
                        pid: None,
                        log_path: None,
                        byte_count: None,
                    },
                ))
            },
            result = run_fut => match result {
                Ok(CommandRunResult::Completed(run)) => {
                    let duration_secs = start.elapsed().as_secs_f64();
                    let output_len = run.output.len();
                    ToolOutcome::success(run.output.clone(), "command completed", duration_secs)
                        .with_metadata(command_metadata(
                            CommandMetadataInput {
                                command: command.clone(),
                                working_dir: Some(effective_workdir.display().to_string()),
                                exit_code: run.exit_code,
                                timed_out: false,
                                background: false,
                                stdout_lines: run.stdout_lines,
                                stderr_lines: run.stderr_lines,
                                detected_urls: all_urls(&run.output),
                                pid: None,
                                log_path: None,
                                byte_count: Some(output_len),
                            },
                        ))
                },
                Ok(CommandRunResult::Detached { pid, log_path }) => {
                    // Ctrl+B moved this command to the background.
                    let duration_secs = start.elapsed().as_secs_f64();
                    let log_path_str = log_path.display().to_string();
                    let output = format!(
                        "Moved to background.\nPID: {pid}\nLog: {log_path_str}\nManage it with /processes, /logs {pid}, /stop {pid}."
                    );
                    let process = ManagedProcess {
                        id: format!("bg-{pid}"),
                        pid,
                        command: command.to_string(),
                        cwd: Some(effective_workdir.display().to_string()),
                        log_path: log_path_str.clone(),
                        detected_url: None,
                        status: ManagedProcessStatus::Running,
                    };
                    let mut metadata = command_metadata(CommandMetadataInput {
                        command: command.to_string(),
                        working_dir: Some(effective_workdir.display().to_string()),
                        exit_code: None,
                        timed_out: false,
                        background: true,
                        stdout_lines: 0,
                        stderr_lines: 0,
                        detected_urls: Vec::new(),
                        pid: Some(pid),
                        log_path: Some(log_path_str),
                        byte_count: Some(output.len()),
                    });
                    metadata.process = Some(process);
                    ToolOutcome::success(output, "moved to background", duration_secs)
                        .with_metadata(metadata)
                },
                Ok(CommandRunResult::Cancelled) => ToolOutcome::cancelled(),
                Err(e) => {
                    let duration_secs = start.elapsed().as_secs_f64();
                    ToolOutcome::error(format!("Command failed: {}", e), duration_secs)
                        .with_metadata(command_metadata(
                            CommandMetadataInput {
                                command: command.clone(),
                                working_dir: Some(effective_workdir.display().to_string()),
                                exit_code: None,
                                timed_out: false,
                                background: false,
                                stdout_lines: 0,
                                stderr_lines: 0,
                                detected_urls: Vec::new(),
                                pid: None,
                                log_path: None,
                                byte_count: None,
                            },
                        ))
                },
            },
        };
        let _ = crate::runtime::run_plugin_hooks(
            "after_shell",
            &serde_json::json!({
                "command": command,
                "status": format!("{:?}", outcome.status),
                "summary": &outcome.summary,
            }),
        );
        outcome
    }
}

#[derive(Debug)]
struct BackgroundStartup {
    ready_message: String,
    log_excerpt: String,
    detected_url: Option<String>,
}

async fn run_background_command(
    command: &str,
    workdir: &Path,
    startup_timeout_secs: u64,
    ready_pattern: Option<&str>,
    open_url: Option<&str>,
    ctx: ExecContext,
) -> ToolOutcome {
    let start = Instant::now();

    {
        let log_path = background_log_path();
        let pid = match launch_background_process(command, workdir, &log_path).await {
            Ok(pid) => pid,
            Err(error) => {
                return ToolOutcome::error(error, start.elapsed().as_secs_f64());
            },
        };

        let startup = match wait_for_background_startup(
            pid,
            &log_path,
            startup_timeout_secs,
            ready_pattern,
            &ctx,
        )
        .await
        {
            Ok(startup) => startup,
            Err(BackgroundWaitError::Cancelled) => {
                let _ = kill_background_process(pid).await;
                return ToolOutcome::cancelled();
            },
            Err(BackgroundWaitError::ExitedEarly(log_excerpt)) => {
                return ToolOutcome::error(
                    format!(
                        "Background command exited during startup. Log: {}\n\n{}",
                        log_path.display(),
                        log_excerpt
                    ),
                    start.elapsed().as_secs_f64(),
                );
            },
        };

        let opened = if let Some(url) = open_url {
            Some((url.to_string(), open_browser_url(url).await))
        } else {
            None
        };

        let mut output = format!(
            "Background command started.\nPID: {}\nLog: {}\n{}\n",
            pid,
            log_path.display(),
            startup.ready_message
        );
        if let Some(url) = startup.detected_url.as_ref() {
            output.push_str(&format!("Detected URL: {}\n", url));
        }
        if let Some((url, result)) = opened {
            match result {
                Ok(()) => output.push_str(&format!("Opened URL: {}\n", url)),
                Err(error) => output.push_str(&format!("Open URL failed: {} ({})\n", url, error)),
            }
        }
        if !startup.log_excerpt.trim().is_empty() {
            output.push_str("\n--- startup output ---\n");
            output.push_str(&startup.log_excerpt);
        }

        let duration_secs = start.elapsed().as_secs_f64();
        let log_path_str = log_path.display().to_string();
        let detected_urls = startup.detected_url.iter().cloned().collect::<Vec<_>>();
        let process = ManagedProcess {
            id: format!("bg-{}", pid),
            pid,
            command: command.to_string(),
            cwd: Some(workdir.display().to_string()),
            log_path: log_path_str.clone(),
            detected_url: startup.detected_url.clone(),
            status: ManagedProcessStatus::Running,
        };
        let byte_count = output.len();
        let mut metadata = command_metadata(CommandMetadataInput {
            command: command.to_string(),
            working_dir: Some(workdir.display().to_string()),
            exit_code: None,
            timed_out: false,
            background: true,
            stdout_lines: startup.log_excerpt.lines().count(),
            stderr_lines: 0,
            detected_urls,
            pid: Some(pid),
            log_path: Some(log_path_str),
            byte_count: Some(byte_count),
        });
        metadata.process = Some(process);
        ToolOutcome::success(output, "background process started", duration_secs)
            .with_metadata(metadata)
    }
}

#[cfg(not(target_os = "windows"))]
async fn launch_background_process(
    command: &str,
    workdir: &Path,
    log_path: &Path,
) -> Result<u32, String> {
    let mut launcher = Command::new("sh");
    launcher
        .arg("-c")
        .arg(
            r#"log=$MERMAID_BG_LOG
cmd=$MERMAID_BG_COMMAND
: > "$log" || exit 125
nohup sh -c "$cmd" > "$log" 2>&1 < /dev/null &
printf '%s\n' "$!""#,
        )
        .env("MERMAID_BG_LOG", log_path)
        .env("MERMAID_BG_COMMAND", command)
        .current_dir(workdir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    scrub_secret_env(&mut launcher);

    let output = launcher
        .output()
        .await
        .map_err(|e| format!("failed to launch background command: {}", e))?;
    if !output.status.success() {
        return Err(format!(
            "background launcher failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.trim().parse::<u32>().map_err(|e| {
        format!(
            "background launcher did not return a pid: {} ({})",
            stdout, e
        )
    })
}

/// Windows: spawn the command detached (no console, own process group) with
/// output redirected to the log file, and return its PID. tokio's `Child`
/// defaults to `kill_on_drop(false)`, so dropping the handle leaves the
/// process running — the OS owns its lifetime from here.
#[cfg(target_os = "windows")]
async fn launch_background_process(
    command: &str,
    workdir: &Path,
    log_path: &Path,
) -> Result<u32, String> {
    // DETACHED_PROCESS: no inherited console. CREATE_NEW_PROCESS_GROUP: not
    // killed when the parent gets Ctrl+C. Together: `cmd /C <command>` keeps
    // running after the tool returns.
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    let log = std::fs::File::create(log_path).map_err(|e| {
        format!(
            "failed to create background log {}: {e}",
            log_path.display()
        )
    })?;
    let log_err = log
        .try_clone()
        .map_err(|e| format!("failed to clone background log handle: {e}"))?;
    let mut launcher = Command::new("cmd");
    launcher
        .arg("/C")
        .arg(command)
        .current_dir(workdir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    scrub_secret_env(&mut launcher);
    let child = launcher
        .spawn()
        .map_err(|e| format!("failed to launch background command: {e}"))?;
    child
        .id()
        .ok_or_else(|| "background command produced no pid".to_string())
}

#[derive(Debug)]
enum BackgroundWaitError {
    Cancelled,
    ExitedEarly(String),
}

async fn wait_for_background_startup(
    pid: u32,
    log_path: &Path,
    startup_timeout_secs: u64,
    ready_pattern: Option<&str>,
    ctx: &ExecContext,
) -> Result<BackgroundStartup, BackgroundWaitError> {
    let start = Instant::now();
    let startup_timeout = Duration::from_secs(startup_timeout_secs);

    loop {
        if ctx.token.is_cancelled() {
            return Err(BackgroundWaitError::Cancelled);
        }

        let last_log = read_log_lossy(log_path).await;
        let detected_url = first_url(&last_log);

        if !process_running(pid).await {
            return Err(BackgroundWaitError::ExitedEarly(tail_lines(&last_log, 40)));
        }

        if let Some(pattern) = ready_pattern {
            if last_log.contains(pattern) {
                return Ok(BackgroundStartup {
                    ready_message: format!("Ready: matched pattern {:?}", pattern),
                    log_excerpt: tail_lines(&last_log, 40),
                    detected_url,
                });
            }
        } else if start.elapsed() >= Duration::from_secs(1) || !last_log.is_empty() {
            return Ok(BackgroundStartup {
                ready_message:
                    "Ready: no ready_pattern provided; process is running after startup check"
                        .to_string(),
                log_excerpt: tail_lines(&last_log, 40),
                detected_url,
            });
        }

        if start.elapsed() >= startup_timeout {
            let ready_message = if let Some(pattern) = ready_pattern {
                format!(
                    "Ready: pattern {:?} was not seen within {}s; process is still running",
                    pattern, startup_timeout_secs
                )
            } else {
                format!(
                    "Ready: startup check reached {}s; process is still running",
                    startup_timeout_secs
                )
            };
            return Ok(BackgroundStartup {
                ready_message,
                log_excerpt: tail_lines(&last_log, 40),
                detected_url,
            });
        }

        tokio::select! {
            _ = ctx.token.cancelled() => return Err(BackgroundWaitError::Cancelled),
            _ = tokio::time::sleep(Duration::from_millis(200)) => {},
        }
    }
}

async fn read_log_lossy(path: &Path) -> String {
    tokio::fs::read_to_string(path).await.unwrap_or_default()
}

#[cfg(not(target_os = "windows"))]
async fn process_running(pid: u32) -> bool {
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Windows: `tasklist` filtered by PID prints the process row only when it
/// exists (otherwise an "INFO: No tasks…" line that doesn't contain the PID).
#[cfg(target_os = "windows")]
async fn process_running(pid: u32) -> bool {
    Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await
        .map(|out| String::from_utf8_lossy(&out.stdout).contains(&pid.to_string()))
        .unwrap_or(false)
}

#[cfg(not(target_os = "windows"))]
async fn kill_background_process(pid: u32) -> std::io::Result<()> {
    let _ = Command::new("kill")
        .arg(pid.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await?;
    Ok(())
}

/// Kill a foreground child AND its descendants on cancel. Windows: `taskkill
/// /T` walks the process tree. Unix: the foreground child is spawned as a
/// process-group leader (`process_group(0)`), so killing the negative pid hits
/// the whole group — catching a grandchild the bare `kill_on_drop` would orphan.
#[cfg(target_os = "windows")]
async fn kill_process_tree(pid: u32) {
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
}

#[cfg(not(target_os = "windows"))]
async fn kill_process_tree(pid: u32) {
    let _ = Command::new("kill")
        .args(["-KILL", "--", &format!("-{pid}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
}

/// Windows: `taskkill /T` kills the whole tree (a `cmd /C` wrapper spawns the
/// real child), `/F` forces it.
#[cfg(target_os = "windows")]
async fn kill_background_process(pid: u32) -> std::io::Result<()> {
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await?;
    Ok(())
}

fn background_log_path() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    std::env::temp_dir().join(format!("mermaid-bg-{}-{}.log", std::process::id(), nanos))
}

struct CommandMetadataInput {
    command: String,
    working_dir: Option<String>,
    exit_code: Option<i32>,
    timed_out: bool,
    background: bool,
    stdout_lines: usize,
    stderr_lines: usize,
    detected_urls: Vec<String>,
    pid: Option<u32>,
    log_path: Option<String>,
    byte_count: Option<usize>,
}

fn command_metadata(input: CommandMetadataInput) -> ToolRunMetadata {
    ToolRunMetadata {
        detail: ToolMetadata::ExecuteCommand {
            command: input.command,
            working_dir: input.working_dir,
            exit_code: input.exit_code,
            timed_out: input.timed_out,
            background: input.background,
            stdout_lines: input.stdout_lines,
            stderr_lines: input.stderr_lines,
            detected_urls: input.detected_urls,
            pid: input.pid,
            log_path: input.log_path,
        },
        line_count: Some(input.stdout_lines + input.stderr_lines),
        byte_count: input.byte_count,
        ..ToolRunMetadata::default()
    }
}

fn tail_lines(text: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..].join("\n")
}

fn first_url(text: &str) -> Option<String> {
    text.split_whitespace()
        .find(|part| part.starts_with("http://") || part.starts_with("https://"))
        .map(|url| {
            url.trim_matches(|c: char| matches!(c, ')' | ']' | '}' | ',' | ';' | '"' | '\''))
                .to_string()
        })
}

fn all_urls(text: &str) -> Vec<String> {
    text.split_whitespace()
        .filter(|part| part.starts_with("http://") || part.starts_with("https://"))
        .map(|url| {
            url.trim_matches(|c: char| matches!(c, ')' | ']' | '}' | ',' | ';' | '"' | '\''))
                .to_string()
        })
        .collect()
}

async fn open_browser_url(url: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut cmd = Command::new("open");
        cmd.arg(url);
        cmd
    };

    #[cfg(target_os = "linux")]
    let mut command = {
        let mut cmd = Command::new("xdg-open");
        cmd.arg(url);
        cmd
    };

    #[cfg(target_os = "windows")]
    let mut command = {
        let mut cmd = Command::new("cmd");
        cmd.args(["/C", "start", "", url]);
        cmd
    };

    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(false)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Drive the child process, pumping stdout+stderr concurrently so
/// the kernel pipe buffer never wedges the child. Emits
/// `ProgressEvent::Output` chunks on `ExecContext::progress` for
/// any future consumer that wants to show live subprocess output.
#[derive(Debug, Clone)]
struct CommandRunOutput {
    output: String,
    exit_code: Option<i32>,
    stdout_lines: usize,
    stderr_lines: usize,
}

/// Result of driving a foreground command: ran to completion, was detached
/// (Ctrl+B), or was cancelled (the turn token fired and we killed the tree).
enum CommandRunResult {
    Completed(CommandRunOutput),
    Detached { pid: u32, log_path: PathBuf },
    Cancelled,
}

/// Names that must never be inherited by a spawned command. Provider API
/// keys + the daemon token live in the parent's environment; a model-driven
/// shell command could otherwise read them via `env`/`printenv` and
/// exfiltrate them. We strip these by exact name in addition to the
/// pattern match in [`scrub_secret_env`].
const SECRET_ENV_VARS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "OLLAMA_API_KEY",
    "GROQ_API_KEY",
    "MISTRAL_API_KEY",
    "DEEPSEEK_API_KEY",
    "OPENROUTER_API_KEY",
    "XAI_API_KEY",
    "TOGETHER_API_KEY",
    "MERMAID_DAEMON_TOKEN",
];

/// Remove secret-bearing environment variables from a child command. Uses a
/// denylist (known provider keys + name patterns) rather than an allowlist so
/// ordinary build/run commands keep `PATH`, `CARGO_HOME`, language toolchain
/// vars, `XAUTHORITY`, etc. and still work.
fn scrub_secret_env(cmd: &mut Command) {
    for (name, _) in std::env::vars() {
        if is_secret_env_name(&name) {
            cmd.env_remove(&name);
        }
    }
}

/// True if an env var name looks like it carries a secret/credential and must
/// not leak into a model-run child process. Denylist (not allowlist) so
/// ordinary build/run vars (`PATH`, toolchain, `XAUTHORITY`, …) survive.
fn is_secret_env_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    SECRET_ENV_VARS.contains(&upper.as_str())
        || upper.contains("API_KEY")
        || upper.contains("APIKEY")
        || upper.contains("ACCESS_KEY")
        || upper.contains("PRIVATE_KEY")
        || upper.contains("SECRET")
        || upper.contains("PASSWORD")
        || upper.contains("PASSWD")
        || upper.contains("CREDENTIAL")
        || upper.contains("TOKEN")
        || upper.contains("WEBHOOK")
        || upper.contains("DATABASE_URL")
        || upper.ends_with("_DSN")
        || upper.contains("CONNECTION_STRING")
        || upper == "KUBECONFIG"
        || upper == "SSH_AUTH_SOCK"
}

/// Drain a child stream, capping the captured bytes at `cap` so a chatty or
/// newline-less command can't exhaust memory. Bytes are accumulated raw and
/// decoded once at the end (lossy) so a multibyte char split across reads is
/// not corrupted by the cap. Returns `(text, truncated)`.
async fn read_capped<R: AsyncRead + Unpin>(
    mut reader: R,
    cap: usize,
    progress: Option<tokio::sync::mpsc::Sender<ProgressEvent>>,
    log: Option<std::sync::Arc<tokio::sync::Mutex<tokio::fs::File>>>,
) -> (String, bool) {
    let mut buf = [0u8; 8192];
    let mut bytes: Vec<u8> = Vec::new();
    let mut truncated = false;
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                // Tee raw bytes to the shared log file so a backgrounded
                // (Ctrl+B) process stays tail-able via /logs.
                if let Some(file) = &log {
                    let mut f = file.lock().await;
                    let _ = f.write_all(&buf[..n]).await;
                    let _ = f.flush().await;
                }
                if let Some(tx) = &progress {
                    let chunk = String::from_utf8_lossy(&buf[..n]);
                    for line in chunk.split('\n') {
                        if !line.is_empty() {
                            let _ = tx.send(ProgressEvent::Output(line.to_string())).await;
                        }
                    }
                }
                if bytes.len() < cap {
                    let take = (cap - bytes.len()).min(n);
                    bytes.extend_from_slice(&buf[..take]);
                    if take < n {
                        truncated = true;
                    }
                } else {
                    truncated = true;
                }
            },
            Err(_) => break,
        }
    }
    let mut out = String::from_utf8_lossy(&bytes).into_owned();
    if truncated {
        out.push_str(&format!("\n…[output truncated at {} bytes]…", cap));
    }
    (out, truncated)
}

async fn run_command(
    mut cmd: Command,
    progress: tokio::sync::mpsc::Sender<ProgressEvent>,
    token: tokio_util::sync::CancellationToken,
    background: tokio_util::sync::CancellationToken,
) -> std::io::Result<CommandRunResult> {
    let mut child = cmd.spawn()?;
    let pid = child.id();

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("child stdout unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| std::io::Error::other("child stderr unavailable"))?;

    // Tee combined output to a log file so that, if the user backgrounds the
    // command (Ctrl+B), it stays tail-able via /logs. Removed on normal exit.
    let log_path = background_log_path();
    let log = tokio::fs::File::create(&log_path)
        .await
        .ok()
        .map(|f| std::sync::Arc::new(tokio::sync::Mutex::new(f)));

    let cap = crate::constants::MAX_TOOL_OUTPUT_BYTES;
    let stdout_task = tokio::spawn(read_capped(
        stdout,
        cap,
        Some(progress.clone()),
        log.clone(),
    ));
    let stderr_task = tokio::spawn(read_capped(stderr, cap, None, log.clone()));

    // A driver task owns the child + drain tasks and runs to completion no
    // matter what. On normal exit it ships the result back. If we detach, we
    // just stop listening — the driver (and its child) keep running, the log
    // keeps filling — until the child exits or Mermaid quits.
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let driver = tokio::spawn(async move {
        let (output, _) = stdout_task.await.unwrap_or_default();
        let (errors, _) = stderr_task.await.unwrap_or_default();
        let status = child.wait().await;
        let _ = done_tx.send((output, errors, status));
    });

    tokio::select! {
        biased;
        _ = background.cancelled() => {
            // Ctrl+B: detach. Dropping `driver`'s JoinHandle does NOT abort the
            // task — it runs on, keeping the child alive and the log filling.
            drop(driver);
            Ok(CommandRunResult::Detached { pid: pid.unwrap_or(0), log_path })
        }
        _ = token.cancelled() => {
            // Turn cancelled (Esc): the detached `driver` would otherwise keep
            // the child (and any grandchild it forked) alive until it exited on
            // its own. Kill the whole tree/group, abort the driver, drop the log.
            if let Some(p) = pid {
                kill_process_tree(p).await;
            }
            driver.abort();
            let _ = tokio::fs::remove_file(&log_path).await;
            Ok(CommandRunResult::Cancelled)
        }
        res = done_rx => {
            // Normal completion — drop the tee log.
            drop(log);
            let _ = tokio::fs::remove_file(&log_path).await;
            let (output, errors, status) = res
                .map_err(|_| std::io::Error::other("command driver dropped before completing"))?;
            let status = status?;
            let stdout_lines = output.lines().count();
            let stderr_lines = errors.lines().count();
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
            Ok(CommandRunResult::Completed(CommandRunOutput {
                output: full_output,
                exit_code: status.code(),
                stdout_lines,
                stderr_lines,
            }))
        }
    }
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

    #[test]
    fn secret_env_name_denylist_covers_common_carriers() {
        // #4: secrets the old denylist missed.
        for name in [
            "ANTHROPIC_API_KEY",
            "AWS_SECRET_ACCESS_KEY",
            "GITHUB_TOKEN",
            "MY_SERVICE_PRIVATE_KEY",
            "DATABASE_URL",
            "SENTRY_DSN",
            "SLACK_WEBHOOK_URL",
            "KUBECONFIG",
            "SSH_AUTH_SOCK",
            "DB_PASSWORD",
            "PG_CONNECTION_STRING",
        ] {
            assert!(is_secret_env_name(name), "{name} should be scrubbed");
        }
        // Ordinary build/run vars must survive.
        for name in [
            "PATH",
            "HOME",
            "CARGO_HOME",
            "LANG",
            "XAUTHORITY",
            "RUSTUP_HOME",
        ] {
            assert!(!is_secret_env_name(name), "{name} should NOT be scrubbed");
        }
    }

    #[tokio::test]
    async fn out_of_project_working_dir_is_escalated_and_blocked() {
        // #1: a read-only command auto-runs in-project, but the same command
        // with an out-of-project working_dir is escalated to ExternalDirectory
        // and denied (here, by ReadOnly mode — proving it's no longer treated
        // as an auto-allowable in-project read).
        let project = std::env::temp_dir().join(format!("mermaid_wd_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&project);
        std::fs::create_dir_all(&project).unwrap();
        let outside = project.parent().unwrap().to_path_buf();

        let mk_ctx = || {
            let (tx, rx) = tokio::sync::mpsc::channel(64);
            let mut config = crate::app::Config::default();
            config.safety.mode = crate::runtime::SafetyMode::ReadOnly;
            let ctx = crate::providers::ctx::ExecContext::new(
                tokio_util::sync::CancellationToken::new(),
                tx,
                ToolCallId(1),
                TurnId(1),
                project.clone(),
                std::sync::Arc::new(config),
                String::new(),
                None,
                crate::runtime::SafetyMode::ReadOnly,
                None,
                None,
                None,
            );
            (ctx, rx)
        };

        let (ctx, _rx) = mk_ctx();
        let outcome = ExecuteCommandTool
            .execute(serde_json::json!({"command": "echo hi"}), ctx)
            .await;
        assert!(
            outcome.is_success(),
            "in-project read-only echo should run: {outcome:?}",
        );

        let (ctx, _rx) = mk_ctx();
        let outcome = ExecuteCommandTool
            .execute(
                serde_json::json!({
                    "command": "echo hi",
                    "working_dir": outside.display().to_string(),
                }),
                ctx,
            )
            .await;
        assert_eq!(
            outcome.status,
            crate::domain::ToolStatus::Error,
            "out-of-project working_dir must be escalated + blocked: {outcome:?}",
        );

        let _ = std::fs::remove_dir_all(&project);
    }

    #[tokio::test]
    async fn safe_command_runs_and_captures_output() {
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), PathBuf::from("/tmp"));
        let outcome = ExecuteCommandTool
            .execute(serde_json::json!({"command": "echo hello world"}), ctx)
            .await;
        assert!(outcome.is_success(), "expected success: {:?}", outcome);
        assert!(outcome.output().contains("hello world"));
    }

    #[tokio::test]
    async fn dangerous_command_blocked() {
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), PathBuf::from("/tmp"));
        let outcome = ExecuteCommandTool
            .execute(serde_json::json!({"command": "rm -rf /"}), ctx)
            .await;
        let error = outcome.error_message().expect("expected error");
        assert!(error.contains("Dangerous"));
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
        // The 5s outer timeout is the real "didn't hang" guard — a propagation
        // regression would block until the 10s sleep, past 5s.
        let outcome = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("didn't hang")
            .expect("join");
        let elapsed = start.elapsed();
        assert!(outcome.was_cancelled());
        // "Aborts promptly", not a hard sub-200ms SLA — a tight bound measured
        // CI scheduling / process-teardown jitter and flaked on loaded windows
        // runners. 2s keeps a wide margin while still catching a real hang.
        assert!(
            elapsed < Duration::from_secs(2),
            "cancellation took {:?} — far slower than expected (regression?)",
            elapsed
        );
    }

    #[tokio::test]
    async fn timeout_honored() {
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), PathBuf::from("/tmp"));
        let outcome = ExecuteCommandTool
            .execute(serde_json::json!({"command": "sleep 5", "timeout": 1}), ctx)
            .await;
        assert_eq!(outcome.status, crate::domain::ToolStatus::Error);
        let output = outcome.as_tool_message_content();
        assert!(output.contains("timed out"));
        assert!(output.contains("was killed"));
        assert!(output.contains("mode=\"background\""));
    }

    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn background_mode_returns_pid_log_and_detected_url() {
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), PathBuf::from("/tmp"));
        let outcome = ExecuteCommandTool
            .execute(
                serde_json::json!({
                    "command": "printf 'ready http://127.0.0.1:54321\\n'; exec sleep 30",
                    "mode": "background",
                    "startup_timeout_secs": 2,
                    "ready_pattern": "ready"
                }),
                ctx,
            )
            .await;

        assert!(
            outcome.is_success(),
            "expected background success: {:?}",
            outcome
        );
        let output = outcome.output().to_string();
        assert!(output.contains("Background command started"));
        assert!(output.contains("PID:"));
        assert!(output.contains("Log:"));
        assert!(output.contains("Ready: matched pattern"));
        assert!(output.contains("Detected URL: http://127.0.0.1:54321"));

        if let Some(pid) = parse_pid(&output) {
            let _ = Command::new("kill").arg(pid.to_string()).status().await;
        }
    }

    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn background_mode_returns_pid_and_log_on_windows() {
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), std::env::temp_dir());
        let outcome = ExecuteCommandTool
            .execute(
                serde_json::json!({
                    "command": "echo ready & ping -n 30 127.0.0.1",
                    "mode": "background",
                    "startup_timeout_secs": 3,
                    "ready_pattern": "ready"
                }),
                ctx,
            )
            .await;

        assert!(
            outcome.is_success(),
            "expected background success on Windows: {:?}",
            outcome
        );
        let output = outcome.output().to_string();
        assert!(output.contains("Background command started"));
        assert!(output.contains("PID:"));
        assert!(output.contains("Ready: matched pattern"));
        // The ManagedProcess must be attached so /processes lists it.
        assert!(
            outcome.metadata.process.is_some(),
            "background outcome must carry a ManagedProcess"
        );

        // Clean up the detached process (and its child ping) via the tree kill.
        if let Some(pid) = parse_pid(&output) {
            let _ = kill_background_process(pid).await;
        }
    }

    #[tokio::test]
    async fn ctrl_b_backgrounds_a_running_foreground_command() {
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), std::env::temp_dir());
        let background = ctx.background.clone();
        // A command that keeps running so it's still live when we background it.
        let command = if cfg!(target_os = "windows") {
            "ping -n 30 127.0.0.1"
        } else {
            "sleep 30"
        };

        // Press "Ctrl+B" shortly after the command starts.
        let canceller = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            background.cancel();
        });
        let outcome = ExecuteCommandTool
            .execute(
                serde_json::json!({ "command": command, "timeout": 60 }),
                ctx,
            )
            .await;
        let _ = canceller.await;

        assert!(
            outcome.is_success(),
            "backgrounding should yield success: {:?}",
            outcome
        );
        let output = outcome.output().to_string();
        assert!(output.contains("Moved to background"), "got: {output}");
        // It must register as a managed process so /processes lists it.
        let process = outcome.metadata.process.clone();
        assert!(
            process.is_some(),
            "background outcome must carry a ManagedProcess"
        );

        // Clean up the still-running detached process (tree kill).
        if let Some(p) = process {
            let _ = kill_background_process(p.pid).await;
        }
    }

    fn parse_pid(output: &str) -> Option<u32> {
        output
            .lines()
            .find_map(|line| line.strip_prefix("PID: "))
            .and_then(|pid| pid.trim().parse().ok())
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
