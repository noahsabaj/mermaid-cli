//! Integration coverage for the `mermaid __sandbox-exec` network denial,
//! end-to-end through the real binary. Linux (seccomp) + macOS (Seatbelt);
//! `#[ignore]`d so it runs in the dedicated integration CI jobs rather than
//! the default suite.
//!
//! The seccomp mechanism itself is unit-tested in `mermaid-runtime::sandbox`
//! (fork + `socket` + assert SIGSYS); this test proves the launcher wiring:
//! that `mermaid __sandbox-exec --no-network -- <cmd>` actually installs the
//! confinement before running the wrapped command.
#![cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]

use std::process::Command;

/// A Python interpreter, if one is on PATH (GitHub's ubuntu and macos runners
/// and most dev machines have `python3`). Returns `None` so the test can skip
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

#[cfg(target_os = "linux")]
const MAKE_INET_SOCKET: &str = "import socket; socket.socket(socket.AF_INET, socket.SOCK_STREAM)";

/// Linux-only: seccomp denies at `socket()` creation. macOS Seatbelt denies at
/// use (connect), not at socket creation, so this assert stays Linux-gated —
/// the connect-based test below covers both platforms.
#[test]
#[cfg(target_os = "linux")]
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

/// Both platforms: an actual TCP connect to a live local listener fails under
/// `--no-network` (Linux: SIGSYS at `socket()`; macOS: EPERM at `connect()`)
/// and succeeds without it.
#[test]
#[ignore = "spawns the real binary + python3; run with: cargo test --test sandbox_network -- --ignored"]
fn no_network_blocks_tcp_connect_but_allows_it_otherwise() {
    let Some(py) = python() else {
        eprintln!("skipping: no python interpreter on PATH");
        return;
    };
    let bin = env!("CARGO_BIN_EXE_mermaid");
    // A real listener; connects complete via the accept backlog, no accept
    // loop needed. Kept alive for the whole test.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind local listener");
    let port = listener.local_addr().expect("local addr").port();
    let connect = format!(
        "import socket; socket.create_connection((\"127.0.0.1\", {port}), timeout=5).close()"
    );

    // Denied: connecting under `--no-network` fails.
    let denied = Command::new(bin)
        .args(["__sandbox-exec", "--no-network", "--", py, "-c", &connect])
        .output()
        .expect("spawn sandboxed python");
    assert!(
        !denied.status.success(),
        "TCP connect should be denied under --no-network (status={:?}, stderr={})",
        denied.status,
        String::from_utf8_lossy(&denied.stderr)
    );

    // Allowed: the same connect without the flag succeeds.
    let allowed = Command::new(bin)
        .args(["__sandbox-exec", "--", py, "-c", &connect])
        .output()
        .expect("spawn unsandboxed python");
    assert!(
        allowed.status.success(),
        "TCP connect should succeed without --no-network (stderr={})",
        String::from_utf8_lossy(&allowed.stderr)
    );
}

#[test]
#[ignore = "spawns the real binary; run with: cargo test --test sandbox_network -- --ignored"]
fn no_network_still_allows_ordinary_local_commands() {
    let bin = env!("CARGO_BIN_EXE_mermaid");
    // A command that only touches the local filesystem must still work under the
    // network denial (it blocks internet sockets, not local I/O).
    let mut cmd = Command::new(bin);
    cmd.args(["__sandbox-exec", "--no-network", "--"]);
    #[cfg(windows)]
    cmd.args(["cmd", "/c", "echo ok"]);
    #[cfg(not(windows))]
    cmd.args(["sh", "-c", "echo ok && ls / >/dev/null"]);

    let output = cmd.output().expect("spawn sandboxed shell");
    assert!(
        output.status.success(),
        "local command should run under --no-network (stderr={})",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
}
