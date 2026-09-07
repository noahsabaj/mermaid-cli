//! The pseudo-terminal run path: the child sees a real console.
use super::*;

/// PTY drain state: tees raw bytes to the log, emits sanitized complete
/// lines as progress, and feeds the bounded capture. One merged stream —
/// a PTY has no stdout/stderr split (`stderr_lines` reports 0).
pub(crate) struct PtyDrain {
    pub(crate) capture: CappedCapture,
    pub(crate) log: Option<std::sync::Arc<tokio::sync::Mutex<tokio::fs::File>>>,
    pub(crate) logged: usize,
    pub(crate) log_capped: bool,
    pub(crate) line_buf: String,
    pub(crate) progress: tokio::sync::mpsc::Sender<ProgressEvent>,
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
/// - NO `setsid` `pre_exec`: on Unix portable-pty already setsids and sets
///   the controlling tty — the child is session+group leader, so
///   `terminate_tree`'s group-kill semantics are byte-identical. On
///   Windows `terminate_tree` kills the tree by pid (`taskkill /T`), so no
///   group setup is needed on either spawn path.
/// - stdin is the pty slave (not /dev/null): a child that READS stdin now
///   hangs to timeout instead of instant EOF — mitigated by
///   `GIT_TERMINAL_PROMPT=0` (still set) and the command timeout.
/// - fixed 24x80 size: nothing resizes it (plumbing the live TUI size is
///   not worth a resize protocol for batch commands).
///
/// Every fallible step happens BEFORE the child spawns, so an `Err` return
/// can safely fall back to the pipe path without re-running side effects —
/// openpty, `clone_reader`, and (Windows) the CPR priming write are the only
/// `?` points ahead of `spawn_command`.
pub(crate) async fn run_command_pty(
    invocation: &ShellInvocation,
    workdir: &Path,
    scratchpad: Option<&Path>,
    progress: tokio::sync::mpsc::Sender<ProgressEvent>,
    token: tokio_util::sync::CancellationToken,
    background: tokio_util::sync::CancellationToken,
    timeout: Duration,
) -> std::io::Result<CommandRunResult> {
    use portable_pty::{PtySize, native_pty_system};

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
    let reader = pair
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

    let mut child = pair
        .slave
        .spawn_command(pty_command(invocation, workdir, scratchpad))
        .map_err(std::io::Error::other)?;
    // Drop the slave so the master reads EOF when the child exits.
    drop(pair.slave);
    let pid = child.process_id();
    let master = pair.master;

    let log_path = background_log_path();
    let log =
        create_tee_log_blocking(&log_path).map(|f| std::sync::Arc::new(tokio::sync::Mutex::new(f)));

    let (reader_thread, drain) = spawn_pty_drain(reader, log, progress);

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
            Ok(CommandRunResult::Completed(pty_completed_output(&raw, &status)))
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

/// The child's command line for the PTY path: the shell invocation, with
/// secrets scrubbed from the environment and the same non-interactive
/// guards the pipe path sets.
fn pty_command(
    invocation: &ShellInvocation,
    workdir: &Path,
    scratchpad: Option<&Path>,
) -> portable_pty::CommandBuilder {
    let mut builder = portable_pty::CommandBuilder::new(&invocation.program);
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
    builder
}

/// A blocking reader thread that feeds pty bytes into a bounded channel, and
/// the async drain that captures them (capped), tees them to the log, and
/// reports progress. The drain resolves to `(output, truncated)`.
fn spawn_pty_drain(
    mut reader: Box<dyn std::io::Read + Send>,
    log: Option<std::sync::Arc<tokio::sync::Mutex<tokio::fs::File>>>,
    progress: tokio::sync::mpsc::Sender<ProgressEvent>,
) -> (
    tokio::task::JoinHandle<()>,
    tokio::task::JoinHandle<(String, bool)>,
) {
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
    (reader_thread, drain)
}

/// Assemble a finished PTY command's output from the raw capture and the
/// exit status portable-pty reported.
fn pty_completed_output(raw: &str, status: &portable_pty::ExitStatus) -> CommandRunOutput {
    // Sanitize the WHOLE capture once (escape sequences can span
    // chunk boundaries; per-chunk stripping is progress-only).
    let mut output = strip_ansi(raw);
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
    CommandRunOutput {
        output,
        exit_code,
        signal,
        // One merged stream on a PTY — there is no stderr split.
        stdout_lines,
        stderr_lines: 0,
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
pub(crate) fn contains_dangerous_command(command: &str) -> bool {
    mermaid_runtime::is_destructive_command(command)
}
