//! Integration coverage for the `mermaid __sandbox-exec` network kill-switch,
//! end-to-end through the real binary. Linux-only (seccomp); `#[ignore]`d so it
//! runs in the dedicated integration CI job rather than the default suite.
//!
//! The seccomp mechanism itself is unit-tested in `mermaid-runtime::sandbox`
//! (fork + `socket` + assert SIGSYS); this test proves the launcher wiring: that
//! `mermaid __sandbox-exec --no-network -- <cmd>` actually installs the filter
//! before exec'ing the wrapped command.
#![cfg(target_os = "linux")]

use std::process::Command;

/// A Python interpreter, if one is on PATH (both GitHub's ubuntu runners and
/// most dev machines have `python3`). Returns `None` so the test can skip
/// cleanly where none exists.
fn python() -> Option<&'static str> {
    ["python3", "python"].into_iter().find(|cand| {
        Command::new(cand)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    })
}

const MAKE_INET_SOCKET: &str = "import socket; socket.socket(socket.AF_INET, socket.SOCK_STREAM)";

#[test]
#[ignore = "spawns the real binary + python3; run with: cargo test --test sandbox_network -- --ignored"]
fn no_network_blocks_inet_socket_but_allows_it_otherwise() {
    let Some(py) = python() else {
        eprintln!("skipping: no python interpreter on PATH");
        return;
    };
    let bin = env!("CARGO_BIN_EXE_mermaid");

    // Denied: creating an internet socket under `--no-network` is killed.
    let denied = Command::new(bin)
        .args([
            "__sandbox-exec",
            "--no-network",
            "--",
            py,
            "-c",
            MAKE_INET_SOCKET,
        ])
        .output()
        .expect("spawn sandboxed python");
    assert!(
        !denied.status.success(),
        "AF_INET socket should be denied under --no-network (status={:?})",
        denied.status
    );

    // Allowed: the same command without the flag succeeds.
    let allowed = Command::new(bin)
        .args(["__sandbox-exec", "--", py, "-c", MAKE_INET_SOCKET])
        .output()
        .expect("spawn unsandboxed python");
    assert!(
        allowed.status.success(),
        "AF_INET socket should succeed without --no-network (stderr={})",
        String::from_utf8_lossy(&allowed.stderr)
    );
}

#[test]
#[ignore = "spawns the real binary; run with: cargo test --test sandbox_network -- --ignored"]
fn no_network_still_allows_ordinary_local_commands() {
    let bin = env!("CARGO_BIN_EXE_mermaid");
    // A command that only touches the local filesystem must still work under the
    // kill-switch (it blocks internet sockets, not AF_UNIX / local I/O).
    let output = Command::new(bin)
        .args([
            "__sandbox-exec",
            "--no-network",
            "--",
            "sh",
            "-c",
            "echo ok && ls / >/dev/null",
        ])
        .output()
        .expect("spawn sandboxed shell");
    assert!(
        output.status.success(),
        "local command should run under --no-network (stderr={})",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
}
