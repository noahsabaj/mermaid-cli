//! Process-tree termination — the single place that knows how to take a spawned
//! child *and its descendants* down.
//!
//! Every tree we manage is spawned as a process-group leader (`process_group(0)`
//! on Unix; `mode=background` uses `setsid`; Windows kills the tree by pid via
//! `taskkill /T`). Terminating the whole group reaches a grandchild that a bare
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

use std::process::Stdio;
use std::time::Duration;

/// How aggressively to tear a tree down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grace {
    /// SIGKILL the group immediately.
    Immediate,
    /// SIGTERM the group, brief grace, then SIGKILL — lets it clean up first.
    Graceful,
}

/// How long `Graceful` waits between SIGTERM and the SIGKILL backstop.
const GRACE_PERIOD: Duration = Duration::from_millis(400);

/// Terminate `pid`'s process tree (async). Safe to call on a pid that has
/// already exited — signals are best-effort and every error is swallowed.
pub async fn terminate_tree(pid: u32, grace: Grace) {
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
