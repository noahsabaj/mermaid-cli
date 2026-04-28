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
use tokio::io::{AsyncBufReadExt, BufReader};
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
                        "description": "Background mode: URL to open with the desktop browser after startup."
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

        let mut policy_request = crate::runtime::ActionRequest::new(
            "execute_command",
            crate::runtime::ToolCategory::Shell,
            command.to_string(),
        );
        policy_request.command = Some(command.to_string());
        let pending_action = serde_json::json!({
            "tool": "execute_command",
            "args": args.clone(),
            "workdir": ctx.workdir.display().to_string(),
            "turn_id": ctx.turn.0,
            "call_id": ctx.call_id.0,
            "task_id": ctx.task_id.clone(),
        });
        match crate::runtime::PolicyEngine::new(ctx.config.safety.mode)
            .with_overrides(ctx.config.safety.overrides.clone())
            .decide(&policy_request)
        {
            crate::runtime::PolicyDecision::Allow { risk, .. } => {
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
            crate::runtime::PolicyDecision::Ask { risk, checkpoint } => {
                let checkpoint_id = if checkpoint && ctx.config.safety.checkpoint_on_mutation {
                    match crate::runtime::create_checkpoint_for_task(
                        &ctx.workdir,
                        &[],
                        Some(pending_action.clone()),
                        ctx.task_id.clone(),
                    ) {
                        Ok(manifest) => Some(manifest.id),
                        Err(error) => {
                            return ToolOutcome::error(
                                format!(
                                    "execute_command checkpoint failed before approval: {}",
                                    error
                                ),
                                0.0,
                            );
                        },
                    }
                } else {
                    None
                };
                let pending_action_json = serde_json::to_string(&pending_action).ok();
                let approval_id = crate::runtime::RuntimeStore::open_default()
                    .and_then(|store| {
                        let approval = store.approvals().create(crate::runtime::NewApproval {
                            task_id: ctx.task_id.clone(),
                            proposed_action: command.to_string(),
                            risk_classification: risk.as_str().to_string(),
                            policy_decision: "ask".to_string(),
                            args_summary: Some(command.to_string()),
                            checkpoint_id: checkpoint_id.clone(),
                            pending_action_json,
                        })?;
                        if let Some(checkpoint_id) = checkpoint_id.as_deref() {
                            let _ = store
                                .checkpoints()
                                .set_approval(checkpoint_id, &approval.id);
                        }
                        let _ = crate::runtime::run_plugin_hooks(
                            "approval_requested",
                            &serde_json::json!({
                                "id": approval.id.clone(),
                                "task_id": approval.task_id.clone(),
                                "tool": "execute_command",
                                "risk": risk.as_str(),
                                "checkpoint_id": checkpoint_id.clone(),
                            }),
                        );
                        Ok(approval)
                    })
                    .map(|approval| approval.id)
                    .ok();
                return ToolOutcome::error(
                    format!(
                        "Approval required for command{}",
                        approval_id
                            .map(|id| format!(" (approval {})", id))
                            .unwrap_or_default()
                    ),
                    0.0,
                );
            },
            crate::runtime::PolicyDecision::Deny { reason, .. } => {
                return ToolOutcome::error(format!("Command blocked by policy: {}", reason), 0.0);
            },
        }

        let mode = match CommandMode::parse(&args) {
            Ok(mode) => mode,
            Err(error) => return ToolOutcome::error(error, 0.0),
        };
        let working_dir = args
            .get("working_dir")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let shell_payload = serde_json::json!({
            "task_id": ctx.task_id.clone(),
            "turn_id": ctx.turn.0,
            "call_id": ctx.call_id.0,
            "command": command,
            "working_dir": working_dir.clone().unwrap_or_else(|| ctx.workdir.display().to_string()),
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
                working_dir.as_deref(),
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

        let outcome = tokio::select! {
            biased;
            _ = ctx.token.cancelled() => ToolOutcome::cancelled(),
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
                        working_dir: working_dir.clone(),
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
                Ok(run) => {
                    let duration_secs = start.elapsed().as_secs_f64();
                    let output_len = run.output.len();
                    ToolOutcome::success(run.output.clone(), "command completed", duration_secs)
                        .with_metadata(command_metadata(
                            CommandMetadataInput {
                                command: command.clone(),
                                working_dir: working_dir.clone(),
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
                Err(e) => {
                    let duration_secs = start.elapsed().as_secs_f64();
                    ToolOutcome::error(format!("Command failed: {}", e), duration_secs)
                        .with_metadata(command_metadata(
                            CommandMetadataInput {
                                command: command.clone(),
                                working_dir: working_dir.clone(),
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
    working_dir: Option<&str>,
    startup_timeout_secs: u64,
    ready_pattern: Option<&str>,
    open_url: Option<&str>,
    ctx: ExecContext,
) -> ToolOutcome {
    let start = Instant::now();

    #[cfg(target_os = "windows")]
    {
        let _ = (
            command,
            working_dir,
            startup_timeout_secs,
            ready_pattern,
            open_url,
            ctx,
        );
        return ToolOutcome::error(
            "execute_command background mode is not supported on Windows yet",
            start.elapsed().as_secs_f64(),
        );
    }

    #[cfg(not(target_os = "windows"))]
    {
        let workdir = working_dir
            .map(PathBuf::from)
            .unwrap_or_else(|| ctx.workdir.clone());
        let log_path = background_log_path();
        let pid = match launch_background_process(command, &workdir, &log_path).await {
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
            working_dir: working_dir.map(str::to_string),
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

#[cfg(not(target_os = "windows"))]
#[derive(Debug)]
enum BackgroundWaitError {
    Cancelled,
    ExitedEarly(String),
}

#[cfg(not(target_os = "windows"))]
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

#[cfg(not(target_os = "windows"))]
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

async fn run_command(
    mut cmd: Command,
    progress: tokio::sync::mpsc::Sender<ProgressEvent>,
) -> std::io::Result<CommandRunOutput> {
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
    Ok(CommandRunOutput {
        output: full_output,
        exit_code: status.code(),
        stdout_lines,
        stderr_lines,
    })
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
        let outcome = tokio::time::timeout(Duration::from_millis(500), handle)
            .await
            .expect("didn't hang")
            .expect("join");
        let elapsed = start.elapsed();
        assert!(outcome.was_cancelled());
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
