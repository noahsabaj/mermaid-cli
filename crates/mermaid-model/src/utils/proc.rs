//! Process plumbing the rest of the app must not reimplement: tree termination
//! (taking a spawned child *and its descendants* down) and bounded,
//! kill-on-timeout subprocess execution.
//!
//! Every tree we manage is spawned as a process-group leader (the exec tool's
//! foreground path uses a `setsid` `pre_exec` — a new *session*, whose leader is
//! also a group leader, so the child additionally loses the controlling
//! terminal; `mode=background` uses `setsid(1)`; mermaid-managed servers use
//! `process_group(0)`; Windows kills the tree by pid via `taskkill /T`).
//! Terminating the whole group reaches a grandchild that a bare
//! per-pid signal would orphan — the bug this module centralizes the fix for: the
//! Esc-cancel path already group-killed, but the foreground timeout, the
//! Ctrl+B-detached cleanup, and the daemon's `/stop`/`/restart` each signalled a
//! single pid.
//!
//! Unix sends to BOTH the group (`-pid`) and the bare pid, so a process that
//! turned out not to be a group leader (e.g. a `mode=background` launch on a host
//! without `setsid`) is still killed directly rather than missed entirely.
//!
//! Callers pick the grace: `Immediate` (Esc-cancel / timeout, which want the
//! fastest possible teardown) or `Graceful` (`/stop`, background cleanup, which
//! give the tree a beat to exit cleanly before the SIGKILL).
//!
//! The bounded-execution half (`output_with_timeout`, `write_stdin_with_timeout`)
//! exists because `std::process` has no deadline primitive: `Command::output()`
//! and `wait()` block until the child chooses to exit. Callers that shell out to
//! helpers which can wedge indefinitely — clipboard tools waiting on a hung
//! selection owner, PowerShell on a cold or broken CLR — use these to turn a
//! potential permanent hang into a bounded stall plus an error.

use std::io::{Read, Write};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// How aggressively to tear a tree down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grace {
    /// SIGKILL the group immediately.
    Immediate,
    /// SIGTERM the group, brief grace, then SIGKILL — lets it clean up first.
    Graceful,
}

/// How long `Graceful` waits between SIGTERM and the SIGKILL backstop.
/// Unix-only like the SIGTERM path itself; Windows teardown has no
/// graceful phase (`taskkill /F` only), so there the constant is dead.
#[cfg(not(target_os = "windows"))]
const GRACE_PERIOD: Duration = Duration::from_millis(400);

/// `CREATE_NEW_PROCESS_GROUP`: the child gets its own process group, so
/// console control events aimed at mermaid's group never reach it.
#[cfg(target_os = "windows")]
pub const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

/// `CREATE_NO_WINDOW`: the child gets its OWN console with **no window**.
/// Paired with [`CREATE_NEW_PROCESS_GROUP`], this is the flag set for every
/// "outlives mermaid" spawn (the exec tool's background launcher, ollama
/// autostart, the managed-search server): the child is exempt from the parent
/// console's Ctrl+C fan-out, survives mermaid exiting, and console APIs still
/// work inside it.
///
/// **Not `DETACHED_PROCESS` (0x8).** That flag is the intuitive choice and it
/// is wrong here. It gives the child *no console at all*, so the moment a
/// console-subsystem child touches console I/O Windows allocates one for it —
/// and on Windows 11, where Windows Terminal is the default console host, that
/// allocation opens a **visible window**. Measured directly: spawning
/// `cmd /C ping … >NUL` with `DETACHED_PROCESS` adds one visible top-level
/// window; the same spawn with `CREATE_NO_WINDOW` adds none. Redirecting stdio
/// to null does not prevent it — the window comes from console allocation, not
/// from output. A killed child then leaves the orphaned window on the user's
/// desktop showing a launch error.
///
/// It is also less compatible: PowerShell dies at startup under
/// `DETACHED_PROCESS` because it requires a console.
///
/// The two flags are mutually exclusive, and there is no case in this codebase
/// that wants `DETACHED_PROCESS`, so the constant is deliberately gone rather
/// than left available to reach for.
#[cfg(target_os = "windows")]
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// pids 0 and 1 are never legitimate teardown targets. On Unix we signal the
/// *process group* `-pid`, so `kill -KILL -- -0` hits our OWN process group
/// (mermaid self-terminates) and `-1` fans out to every process we may signal.
/// Real managed children always have pid > 1; anything at or below 1 is a
/// phantom (e.g. a `ManagedProcess` that ever recorded pid 0) and must be a
/// no-op so a stray `/stop` can't take the app — or the box — down with it.
fn is_signalable(pid: u32) -> bool {
    pid > 1
}

/// Terminate `pid`'s process tree (async). Safe to call on a pid that has
/// already exited — signals are best-effort and every error is swallowed.
pub async fn terminate_tree(pid: u32, grace: Grace) {
    if !is_signalable(pid) {
        return;
    }
    #[cfg(not(target_os = "windows"))]
    {
        if grace == Grace::Graceful {
            unix_kill("-TERM", pid).await;
            tokio::time::sleep(GRACE_PERIOD).await;
        }
        unix_kill("-KILL", pid).await;
    }
    #[cfg(target_os = "windows")]
    {
        let _ = grace; // taskkill /F is always forceful.
        taskkill_tree(pid).await;
    }
}

/// Blocking sibling for sync call sites (the daemon's `/stop` / `/restart` run
/// off a sync runtime client and can't await).
pub fn terminate_tree_blocking(pid: u32, grace: Grace) {
    if !is_signalable(pid) {
        return;
    }
    #[cfg(not(target_os = "windows"))]
    {
        if grace == Grace::Graceful {
            unix_kill_blocking("-TERM", pid);
            std::thread::sleep(GRACE_PERIOD);
        }
        unix_kill_blocking("-KILL", pid);
    }
    #[cfg(target_os = "windows")]
    {
        let _ = grace;
        taskkill_tree_blocking(pid);
    }
}

#[cfg(not(target_os = "windows"))]
async fn unix_kill(signal: &str, pid: u32) {
    let _ = tokio::process::Command::new("kill")
        .args([signal, "--", &format!("-{pid}"), &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
}

#[cfg(not(target_os = "windows"))]
fn unix_kill_blocking(signal: &str, pid: u32) {
    let _ = std::process::Command::new("kill")
        .args([signal, "--", &format!("-{pid}"), &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(target_os = "windows")]
async fn taskkill_tree(pid: u32) {
    let _ = tokio::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
}

#[cfg(target_os = "windows")]
fn taskkill_tree_blocking(pid: u32) {
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

// ---------------------------------------------------------------------------
// Bounded subprocess execution
// ---------------------------------------------------------------------------

/// Cadence for deadline-armed `try_wait` polling. Cheap enough to leave tight —
/// it bounds how much latency the deadline machinery adds to a fast child.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// After a child exits, how long `output_with_timeout` waits for its pipes to
/// hit EOF. Normally EOF is immediate; the grace only bites when a grandchild
/// inherited the pipe and outlives the child — partial output is returned.
const READER_GRACE: Duration = Duration::from_millis(250);

/// Bounded reap window after a deadline kill. SIGKILL can't be ignored, but a
/// child stuck in uninterruptible I/O (dead X socket, hung NFS) may not die
/// promptly — and trading the caller's hang for ours would defeat the point.
const REAP_GRACE: Duration = Duration::from_millis(500);

/// Run `cmd` to completion with a deadline, capturing stdout/stderr — a
/// `Command::output()` that cannot hang. On expiry the child is killed and
/// (bounded-best-effort) reaped, and `ErrorKind::TimedOut` comes back. stdin
/// is null.
///
/// Pipes are drained on background threads, so a child that writes more than
/// a pipe buffer's worth can't deadlock against the wait loop.
pub fn output_with_timeout(cmd: &mut Command, timeout: Duration) -> std::io::Result<Output> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let stdout_rx = drain_pipe(child.stdout.take());
    let stderr_rx = drain_pipe(child.stderr.take());

    match wait_deadline(&mut child, timeout)? {
        Some(status) => Ok(Output {
            status,
            stdout: collect_drained(stdout_rx),
            stderr: collect_drained(stderr_rx),
        }),
        None => Err(timed_out(cmd, timeout)),
    }
}

/// Feed `input` to `cmd`'s stdin and wait for exit under a deadline — the
/// write-side sibling of [`output_with_timeout`]. stdout/stderr go to null,
/// so tools that fork a long-lived holder of their fds (`wl-copy` and `xclip`
/// serve the selection from a background fork) can't pin any pipe of ours.
///
/// stdin is fed from a background thread: a child that never reads can't
/// block the caller, and dropping the handle after the write delivers EOF.
/// A write against a dead child (EPIPE) is ignored — the exit status or the
/// timeout already tells that story.
pub fn write_stdin_with_timeout(
    cmd: &mut Command,
    input: Vec<u8>,
    timeout: Duration,
) -> std::io::Result<ExitStatus> {
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = cmd.spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        std::thread::spawn(move || {
            let _ = stdin.write_all(&input);
            // Dropping `stdin` closes the pipe — the child sees EOF.
        });
    }
    match wait_deadline(&mut child, timeout)? {
        Some(status) => Ok(status),
        None => Err(timed_out(cmd, timeout)),
    }
}

/// Poll-wait for `child` up to `timeout`. `Ok(None)` means the deadline
/// passed: the child has been killed and reaping was attempted for
/// [`REAP_GRACE`]. An unreapable child (unkillable, stuck in uninterruptible
/// I/O) lingers as a zombie until this process exits — accepted for that
/// pathological case rather than risking a blocking `wait()` here.
fn wait_deadline(child: &mut Child, timeout: Duration) -> std::io::Result<Option<ExitStatus>> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let reap_deadline = Instant::now() + REAP_GRACE;
            while Instant::now() < reap_deadline {
                if matches!(child.try_wait(), Ok(Some(_)) | Err(_)) {
                    break;
                }
                std::thread::sleep(POLL_INTERVAL);
            }
            return Ok(None);
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Stream a pipe's bytes over a channel from a background thread. The thread
/// exits on EOF or when the receiver is gone; chunking (rather than one
/// read-to-end) means a pipe held open past child exit still yields whatever
/// was written before it.
fn drain_pipe<R: Read + Send + 'static>(pipe: Option<R>) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    if let Some(mut pipe) = pipe {
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match pipe.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    },
                }
            }
        });
    }
    rx
}

/// Gather everything a [`drain_pipe`] thread produced. Returns as soon as the
/// pipe hits EOF (sender dropped); otherwise gives up after [`READER_GRACE`]
/// so a grandchild that inherited the pipe can't stall us, keeping any
/// partial output.
fn collect_drained(rx: mpsc::Receiver<Vec<u8>>) -> Vec<u8> {
    let mut out = Vec::new();
    let deadline = Instant::now() + READER_GRACE;
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        match rx.recv_timeout(remaining) {
            Ok(chunk) => out.extend_from_slice(&chunk),
            // Disconnected (EOF) or Timeout (grace expired) — either way, done.
            Err(_) => break,
        }
    }
    out
}

/// The error a deadline kill surfaces: `ErrorKind::TimedOut`, naming the
/// program so an `anyhow` context chain reads well in the status line.
fn timed_out(cmd: &Command, timeout: Duration) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!(
            "{} did not exit within {timeout:?} and was killed",
            cmd.get_program().to_string_lossy()
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pids_0_and_1_are_never_signalable() {
        // The dangerous shapes: `-0` is our own process group, `-1` is every
        // signalable process. Neither may ever reach a real `kill`.
        assert!(!is_signalable(0));
        assert!(!is_signalable(1));
    }

    #[test]
    fn real_pids_are_signalable() {
        assert!(is_signalable(2));
        assert!(is_signalable(1234));
        assert!(is_signalable(u32::MAX));
    }

    #[tokio::test]
    async fn terminate_tree_is_a_noop_for_unsafe_pids() {
        // Must return promptly without shelling out to `kill` — the guard runs
        // before any process spawn. (If it did signal, it would target our own
        // group; the test process surviving is the assertion.)
        terminate_tree(0, Grace::Immediate).await;
        terminate_tree(1, Grace::Graceful).await;
        terminate_tree_blocking(0, Grace::Immediate);
        terminate_tree_blocking(1, Grace::Graceful);
    }

    // ---- bounded execution ----

    #[cfg(unix)]
    fn sh(script: &str) -> Command {
        let mut c = Command::new("sh");
        c.args(["-c", script]);
        c
    }

    #[cfg(windows)]
    fn cmd_c(script: &str) -> Command {
        let mut c = Command::new("cmd");
        c.args(["/C", script]);
        c
    }

    #[cfg(unix)]
    #[test]
    fn output_with_timeout_captures_output() {
        let out = output_with_timeout(&mut sh("echo hello"), Duration::from_secs(10)).unwrap();
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hello");
    }

    #[cfg(unix)]
    #[test]
    fn output_with_timeout_kills_a_hung_child() {
        let start = Instant::now();
        let err = output_with_timeout(&mut sh("sleep 30"), Duration::from_millis(200))
            .expect_err("a hung child must surface as an error");
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "deadline kill must not wait for the child's natural exit"
        );
    }

    #[cfg(unix)]
    #[test]
    fn output_with_timeout_drains_more_than_a_pipe_buffer() {
        // 1 MiB ≫ the ~64 KiB pipe buffer: without background draining the
        // child blocks on a full pipe and the wait loop would time out.
        let out = output_with_timeout(
            &mut sh("head -c 1048576 /dev/zero"),
            Duration::from_secs(10),
        )
        .unwrap();
        assert!(out.status.success());
        assert_eq!(out.stdout.len(), 1_048_576);
    }

    #[cfg(unix)]
    #[test]
    fn output_with_timeout_tolerates_a_grandchild_holding_the_pipe() {
        // The child exits immediately but leaves a background grandchild
        // holding the inherited stdout pipe open (the `wl-copy`/`xclip`
        // serve-the-selection pattern). Output written before the exit must
        // still come back, within READER_GRACE rather than the grandchild's
        // lifetime.
        let start = Instant::now();
        let out = output_with_timeout(
            &mut sh("echo early; sleep 5 & exit 0"),
            Duration::from_secs(10),
        )
        .unwrap();
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "early");
        assert!(
            start.elapsed() < Duration::from_secs(4),
            "an fd-holding grandchild must not stall collection"
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_stdin_with_timeout_feeds_the_child() {
        // The child's exit status proves the bytes arrived and EOF followed.
        let status = write_stdin_with_timeout(
            &mut sh(r#"input=$(cat); [ "$input" = "hello" ]"#),
            b"hello".to_vec(),
            Duration::from_secs(10),
        )
        .unwrap();
        assert!(status.success());
    }

    #[cfg(unix)]
    #[test]
    fn write_stdin_with_timeout_kills_a_child_that_never_reads() {
        // Input larger than the pipe buffer, against a child that never reads
        // stdin: the writer thread blocks on the full pipe and must be freed
        // by the deadline kill (EPIPE), not wedge anything.
        let start = Instant::now();
        let err = write_stdin_with_timeout(
            &mut sh("sleep 30"),
            vec![b'x'; 1_048_576],
            Duration::from_millis(200),
        )
        .expect_err("a child that never reads must time out");
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        assert!(start.elapsed() < Duration::from_secs(10));
    }

    /// Every long-lived Windows spawn must use `CREATE_NO_WINDOW`, never
    /// `DETACHED_PROCESS`.
    ///
    /// This is a source check rather than a behavioral one because the symptom
    /// is a *desktop* artifact, not a process property: `DETACHED_PROCESS`
    /// leaves the child with no console, Windows allocates one on first console
    /// I/O, and on Windows 11 that opens a real Terminal window. A killed child
    /// then orphans it. The test suite itself did this on every run — the bug
    /// was found by noticing a stack of dead `ping -n 31 127.0.0.1` windows on
    /// a developer's desktop, which no assertion in this suite could have seen.
    #[cfg(windows)]
    #[test]
    fn no_spawn_site_uses_detached_process() {
        // Split so this scanner's own source never contains the whole token —
        // otherwise the guard reports itself.
        let needle = concat!("DETACHED", "_PROCESS");
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        let mut stack = vec![root];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().is_none_or(|ext| ext != "rs") {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                for (number, line) in text.lines().enumerate() {
                    // Prose (this doc comment, the constant's docs) explains why
                    // the flag is banned; only real code counts.
                    let code = line.trim_start();
                    if code.starts_with("//") || code.starts_with("///") {
                        continue;
                    }
                    if line.contains(needle) {
                        offenders.push(format!("{}:{}", path.display(), number + 1));
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "{needle} opens a visible console window on Windows 11 — use \
             CREATE_NO_WINDOW instead. Offending lines: {offenders:?}",
        );
    }

    #[cfg(windows)]
    #[test]
    fn output_with_timeout_captures_output() {
        let out = output_with_timeout(&mut cmd_c("echo hello"), Duration::from_secs(20)).unwrap();
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hello");
    }

    #[cfg(windows)]
    #[test]
    fn output_with_timeout_kills_a_hung_child() {
        let start = Instant::now();
        let err = output_with_timeout(
            &mut cmd_c("ping -n 30 127.0.0.1"),
            Duration::from_millis(500),
        )
        .expect_err("a hung child must surface as an error");
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "deadline kill must not wait for the child's natural exit"
        );
    }

    #[cfg(windows)]
    #[test]
    fn write_stdin_with_timeout_feeds_the_child() {
        // findstr exits 0 iff a line of stdin matches — proves delivery + EOF.
        let status = write_stdin_with_timeout(
            &mut cmd_c("findstr hello"),
            b"hello\r\n".to_vec(),
            Duration::from_secs(20),
        )
        .unwrap();
        assert!(status.success());
    }
}
