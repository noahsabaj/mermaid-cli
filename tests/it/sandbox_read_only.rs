//! Integration coverage for read-only containment, end to end through the real
//! binary: the `mermaid __sandbox-exec --read-only` launcher, and the
//! `execute_command` tool in `read_only` mode, which lets any command run
//! because the OS keeps it read-only. Linux (Landlock ABI 6 + seccomp) and
//! macOS (a deny-default Seatbelt profile). On Linux each test skips on a
//! kernel that cannot enforce it, where `read_only` mode keeps the shell
//! allowlists; on macOS the profile must work, so the tests fail instead.
//! `#[ignore]`d so they run in the dedicated integration CI jobs rather than
//! the default suite.
//!
//! The Linux mechanism itself is unit-tested in `mermaid-runtime::sandbox`
//! (fork, apply, probe each denial); this proves the wiring, and on macOS it
//! is the only place the profile meets a real kernel.
#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Whether this machine can contain `read_only` commands. A Linux kernel
/// below 6.12 cannot, and the test skips; macOS always must.
fn containment_available() -> bool {
    let available = mermaid_runtime::read_only_containment_available();
    assert!(
        available || cfg!(target_os = "linux"),
        "the read-only Seatbelt profile did not run a shell on this macOS"
    );
    if !available {
        eprintln!("skipping: kernel cannot enforce read-only containment");
    }
    available
}

/// Run `script` under `sh -c` through the read-only launcher, in `dir`.
fn contained_sh(dir: &Path, script: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_mermaid"))
        .args(["__sandbox-exec", "--read-only", "--", "sh", "-c", script])
        .current_dir(dir)
        .output()
        .expect("spawn sandboxed shell")
}

fn fresh_base(tag: &str) -> PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time before epoch")
        .as_nanos();
    let base = std::env::temp_dir().join(format!(
        "mermaid-sandbox-ro-{tag}-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir_all(&base).expect("create test base dir");
    base
}

#[test]
#[ignore = "spawns the real binary; run with: cargo nextest run --test integration --run-ignored only it::sandbox_read_only::"]
fn launcher_reads_but_cannot_write() {
    if !containment_available() {
        return;
    }
    let base = fresh_base("launcher");
    std::fs::write(base.join("existing.txt"), "hello").unwrap();

    let output = contained_sh(
        &base,
        "cat existing.txt && ls >/dev/null && echo x > created.txt",
    );
    let created = base.join("created.txt").exists();
    let _ = std::fs::remove_dir_all(&base);

    assert_eq!(String::from_utf8_lossy(&output.stdout), "hello");
    assert!(
        !output.status.success(),
        "the write should fail (stderr={})",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!created, "a read-only command created a file");
}

/// Changes that are not file writes: permission bits, a TCP connection to a
/// live local listener, and a signal to a process outside the sandbox. Each
/// fails, and a plain read beside them still works.
#[test]
#[ignore = "spawns the real binary; run with: cargo nextest run --test integration --run-ignored only it::sandbox_read_only::"]
fn launcher_denies_changes_that_are_not_file_writes() {
    if !containment_available() {
        return;
    }
    let base = fresh_base("beyond");
    let file = base.join("existing.txt");
    std::fs::write(&file, "hello").unwrap();
    let mode_before = std::fs::metadata(&file).unwrap().permissions();

    let chmod = contained_sh(&base, "chmod 000 existing.txt");
    // The parent of the contained shell is this test process, which is
    // outside the sandbox.
    let signal = contained_sh(&base, "kill -0 $PPID");
    let read = contained_sh(&base, "wc -c < existing.txt");
    let mode_after = std::fs::metadata(&file).unwrap().permissions();
    let _ = std::fs::remove_dir_all(&base);

    assert!(!chmod.status.success(), "chmod should fail");
    assert_eq!(
        mode_before, mode_after,
        "a read-only command changed a mode"
    );
    assert!(
        !signal.status.success(),
        "a signal left the sandbox (stderr={})",
        String::from_utf8_lossy(&signal.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&read.stdout).trim(), "5");

    let Some(py) = ["python3", "python"].into_iter().find(|cand| {
        Command::new(cand)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }) else {
        eprintln!("skipping the network check: no python interpreter on PATH");
        return;
    };
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind local listener");
    let port = listener.local_addr().expect("local addr").port();
    let connect = format!(
        "import socket; socket.create_connection((\"127.0.0.1\", {port}), timeout=5).close()"
    );
    let contained = Command::new(env!("CARGO_BIN_EXE_mermaid"))
        .args(["__sandbox-exec", "--read-only", "--", py, "-c", &connect])
        .output()
        .expect("spawn sandboxed python");
    assert!(
        !contained.status.success(),
        "a read-only command opened a TCP connection (stderr={})",
        String::from_utf8_lossy(&contained.stderr)
    );
}

/// macOS keeps preferences in a daemon, so a preference write is a change
/// that never touches a file from the command's side. The profile lets reads
/// reach the daemon; the daemon must refuse the write.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "spawns the real binary; run with: cargo nextest run --test integration --run-ignored only it::sandbox_read_only::"]
fn launcher_cannot_write_preferences() {
    if !containment_available() {
        return;
    }
    let domain = format!("dev.mermaid.sandbox-test.{}", std::process::id());
    let base = fresh_base("defaults");
    let write = contained_sh(
        &base,
        &format!("defaults write {domain} key -string changed"),
    );
    let read_back = Command::new("defaults")
        .args(["read", &domain, "key"])
        .output()
        .expect("run defaults read");
    let _ = Command::new("defaults").args(["delete", &domain]).output();
    let _ = std::fs::remove_dir_all(&base);

    assert!(
        !read_back.status.success(),
        "a read-only command wrote a preference (write status={:?}, stderr={})",
        write.status,
        String::from_utf8_lossy(&write.stderr)
    );
}

/// `touch` is a mutation the allowlists deny outright in `read_only`. With
/// containment it runs, the kernel refuses the write, and the model is told
/// why; a plain read in the same mode still succeeds.
#[test]
#[ignore = "spawns the real binary; run with: cargo nextest run --test integration --run-ignored only it::sandbox_read_only::"]
fn read_only_mode_runs_commands_contained() {
    use mermaid_cli::providers::ToolExecutor;
    use mermaid_cli::providers::ctx::test_exec_context_with_config;
    use mermaid_cli::providers::tool::exec::ExecuteCommandTool;
    use mermaid_domain::{ToolCallId, TurnId};

    if !containment_available() {
        return;
    }
    // SAFETY: names the launcher for every command this test binary's tool
    // spawns; nothing reads it concurrently in a way that matters.
    unsafe {
        std::env::set_var("MERMAID_LAUNCHER_EXE", env!("CARGO_BIN_EXE_mermaid"));
    }
    let base = fresh_base("tool");
    std::fs::write(base.join("existing.txt"), "hello").unwrap();
    let mut config = mermaid_domain::Config::default();
    config.safety.mode = mermaid_runtime::SafetyMode::ReadOnly;
    let rt = tokio::runtime::Runtime::new().unwrap();
    let run = |command: &str| {
        let (ctx, _rx) =
            test_exec_context_with_config(TurnId(1), ToolCallId(1), base.clone(), config.clone());
        let outcome =
            rt.block_on(ExecuteCommandTool.execute(serde_json::json!({ "command": command }), ctx));
        (outcome.is_success(), outcome.output().to_string())
    };

    let (write_ok, write_output) = run("touch created.txt");
    let (read_ok, read_output) = run("cat existing.txt");
    let created = base.join("created.txt").exists();
    let _ = std::fs::remove_dir_all(&base);

    assert!(!write_ok, "the write should fail: {write_output}");
    assert!(
        write_output.contains("read-only sandbox"),
        "the model is told the sandbox refused it, not the policy: {write_output}"
    );
    assert!(!created, "a read_only-mode command created a file");
    assert!(read_ok && read_output.contains("hello"), "{read_output}");
}
