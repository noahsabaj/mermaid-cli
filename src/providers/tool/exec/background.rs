//! Background (`mode="background"`) process launch, startup wait, and liveness.
//!
//! The reason this is its own file: `launch_background_process` and
//! `process_running` each have a `#[cfg(unix)]` and a `#[cfg(windows)]`
//! definition, and they were interleaved with unrelated code 70 lines apart.
//! A Windows-only change is now reviewable in one place.
use super::*;

#[derive(Debug)]
pub(crate) struct BackgroundStartup {
    pub(crate) ready_message: String,
    pub(crate) log_excerpt: String,
    pub(crate) detected_url: Option<String>,
}

pub(crate) async fn run_background_command(
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
pub(crate) async fn launch_background_process(
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
pub(crate) async fn launch_background_process(
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
pub(crate) enum BackgroundWaitError {
    Cancelled,
    ExitedEarly(String),
}

pub(crate) async fn wait_for_background_startup(
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

pub(crate) async fn read_log_lossy(path: &Path) -> String {
    tokio::fs::read_to_string(path).await.unwrap_or_default()
}

#[cfg(not(target_os = "windows"))]
pub(crate) async fn process_running(pid: u32) -> bool {
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
pub(crate) async fn process_running(pid: u32) -> bool {
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
pub(crate) fn background_log_path() -> PathBuf {
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
pub(crate) fn create_log_file_blocking(path: &Path) -> std::io::Result<std::fs::File> {
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
pub(crate) fn create_tee_log_blocking(path: &Path) -> Option<tokio::fs::File> {
    #[cfg(unix)]
    let std_file = create_log_file_blocking(path).ok();
    #[cfg(not(unix))]
    let std_file = std::fs::File::create(path).ok();
    std_file.map(tokio::fs::File::from_std)
}
