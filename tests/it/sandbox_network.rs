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
#[ignore = "spawns the real binary + python3; run with: cargo test --test integration -- --ignored it::sandbox_network::"]
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
#[ignore = "spawns the real binary + python3; run with: cargo test --test integration -- --ignored it::sandbox_network::"]
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
#[ignore = "spawns the real binary; run with: cargo test --test integration -- --ignored it::sandbox_network::"]
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

/// The kill-switch reaches a `mode="background"` command. The background
/// launcher used to spawn `sh -c` bare, with the `__sandbox-exec` wrapping
/// computed only on the foreground path after the background early return,
/// so `--no-network` did not apply to a backgrounded command at all. The
/// tool is driven directly with `safety.network = deny`; the launcher binary
/// is the real `mermaid` (a test process has no `__sandbox-exec`).
#[test]
#[ignore = "spawns the real binary + python3; run with: cargo test --test integration -- --ignored it::sandbox_network::"]
fn no_network_confines_a_background_command_too() {
    use mermaid_cli::providers::ToolExecutor;
    use mermaid_cli::providers::ctx::test_exec_context_with_config;
    use mermaid_cli::providers::tool::exec::ExecuteCommandTool;
    use mermaid_domain::{NetworkPolicy, ToolCallId, TurnId};

    let Some(py) = python() else {
        eprintln!("skipping: no python interpreter on PATH");
        return;
    };
    // SAFETY: this test binary is single-purpose; nothing else reads the
    // variable concurrently in a way that matters, and it names the launcher
    // for every command the tool spawns from here on.
    unsafe {
        std::env::set_var("MERMAID_LAUNCHER_EXE", env!("CARGO_BIN_EXE_mermaid"));
    }
    let workdir = std::env::temp_dir().join(format!("mermaid-bg-sandbox-{}", std::process::id()));
    std::fs::create_dir_all(&workdir).unwrap();

    let mut config = mermaid_domain::Config::default();
    config.safety.mode = mermaid_runtime::SafetyMode::FullAccess;
    config.safety.network = NetworkPolicy::Deny;
    let (ctx, _rx) =
        test_exec_context_with_config(TurnId(1), ToolCallId(1), workdir.clone(), config);

    // The probe's result lands in the background log as one word.
    let command = format!(
        "if {py} -c \"{MAKE_INET_SOCKET}\" 2>/dev/null; then echo NET_ALLOWED; else echo NET_DENIED; fi; sleep 20"
    );
    let rt = tokio::runtime::Runtime::new().unwrap();
    let outcome = rt.block_on(ExecuteCommandTool.execute(
        serde_json::json!({
            "command": command,
            "mode": "background",
            "startup_timeout_secs": 5,
            "ready_pattern": "NET_"
        }),
        ctx,
    ));
    let output = outcome.output().to_string();
    let log_path = output
        .lines()
        .find_map(|l| l.trim().strip_prefix("Log: "))
        .map(str::trim)
        .expect("background output names its log");
    let log = std::fs::read_to_string(log_path).unwrap_or_default();
    if let Some(pid) = output
        .lines()
        .find_map(|l| l.trim().strip_prefix("PID: "))
        .and_then(|p| p.trim().parse::<u32>().ok())
    {
        let _ = Command::new("kill").arg(pid.to_string()).status();
    }
    let _ = std::fs::remove_dir_all(&workdir);
    assert!(outcome.is_success(), "background start failed: {output}");
    assert!(
        log.contains("NET_DENIED") && !log.contains("NET_ALLOWED"),
        "a backgrounded command reached the network under --no-network; log: {log}"
    );
}
