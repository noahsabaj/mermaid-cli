//! Capped output capture, secret-scrubbed environments, and the pipe run path.
use super::*;

/// Drive the child process, pumping stdout+stderr concurrently so
/// the kernel pipe buffer never wedges the child. Emits
/// `ProgressEvent::Output` chunks on `ExecContext::progress` for
/// any future consumer that wants to show live subprocess output.
#[derive(Debug, Clone)]
pub(crate) struct CommandRunOutput {
    pub(crate) output: String,
    pub(crate) exit_code: Option<i32>,
    /// Terminating signal (Unix), when the process was killed by one — e.g.
    /// SIGSYS from the seccomp network kill-switch. `None` on a normal exit or
    /// on non-Unix.
    pub(crate) signal: Option<i32>,
    pub(crate) stdout_lines: usize,
    pub(crate) stderr_lines: usize,
}

/// Result of driving a foreground command: ran to completion, was detached
/// (Ctrl+B), was cancelled (the turn token fired), or hit its timeout. The
/// cancelled and timed-out arms both tree-kill the process group and abort the
/// driver before returning, so neither can leak the child.
pub(crate) enum CommandRunResult {
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
pub(crate) const SECRET_ENV_VARS: &[&str] = &[
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
pub(crate) fn harden_noninteractive_env(cmd: &mut Command) {
    cmd.env("GIT_TERMINAL_PROMPT", "0");
}

/// Remove secret-bearing environment variables from a child command. Uses a
/// denylist (known provider keys + name patterns) rather than an allowlist so
/// ordinary build/run commands keep `PATH`, `CARGO_HOME`, language toolchain
/// vars, `XAUTHORITY`, etc. and still work.
pub(crate) fn scrub_secret_env(cmd: &mut Command) {
    for name in secret_env_names() {
        cmd.env_remove(&name);
    }
}

/// The concrete secret-bearing names present in THIS process's environment —
/// shared by the pipe path (`scrub_secret_env`) and the PTY path
/// (`CommandBuilder::env_remove`), so the two spawn paths can't drift.
pub(crate) fn secret_env_names() -> Vec<String> {
    std::env::vars()
        .map(|(name, _)| name)
        .filter(|name| is_secret_env_name(name))
        .collect()
}

/// True if an env var name looks like it carries a secret/credential and must
/// not leak into a model-run child process. Denylist (not allowlist) so
/// ordinary build/run vars (`PATH`, toolchain, `XAUTHORITY`, …) survive.
pub(crate) fn is_secret_env_name(name: &str) -> bool {
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
pub(crate) const TEE_LOG_CAP_BYTES: usize = 64 * 1024 * 1024;

/// Bounded head+tail capture core, shared by the pipe reader (`read_capped`)
/// and the PTY drain. Keeps the HEAD (up to cap/2) and a bounded TAIL ring:
/// command output puts the actual error / exit summary at the END, which
/// head-only truncation used to discard. head_cap + tail_cap == cap, so any
/// total <= cap reconstructs exactly (no marker); only a genuine overflow
/// drops the middle.
pub(crate) struct CappedCapture {
    pub(crate) head_cap: usize,
    pub(crate) tail_cap: usize,
    pub(crate) head: Vec<u8>,
    pub(crate) tail: std::collections::VecDeque<u8>,
    pub(crate) total: usize,
}

impl CappedCapture {
    pub(crate) fn new(cap: usize) -> Self {
        let head_cap = cap / 2;
        Self {
            head_cap,
            tail_cap: cap - head_cap,
            head: Vec::new(),
            tail: std::collections::VecDeque::new(),
            total: 0,
        }
    }

    pub(crate) fn push(&mut self, mut chunk: &[u8]) {
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
    pub(crate) fn finish(self) -> (String, bool) {
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

pub(crate) async fn read_capped<R: AsyncRead + Unpin>(
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
pub(crate) fn strip_ansi(input: &str) -> String {
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
pub(crate) async fn run_command(
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
