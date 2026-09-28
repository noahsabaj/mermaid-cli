//! Integration coverage for read-only containment, end to end through the real
//! binary: the `mermaid __sandbox-exec --read-only` launcher, and the
//! `execute_command` tool in `read_only` mode, which now lets any command run
//! because the OS keeps it read-only. Linux only (Landlock ABI 6 + seccomp);
//! each test skips on a kernel that cannot enforce it, where `read_only` mode
//! keeps the shell allowlists. `#[ignore]`d so it runs in the dedicated
//! integration CI job rather than the default suite.
//!
//! The mechanism itself is unit-tested in `mermaid-runtime::sandbox` (fork,
//! apply, probe each denial); this proves the wiring.
#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::process::Command;

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
    if !mermaid_runtime::read_only_containment_available() {
        eprintln!("skipping: kernel cannot enforce read-only containment");
        return;
    }
    let base = fresh_base("launcher");
    std::fs::write(base.join("existing.txt"), "hello").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_mermaid"))
        .args(["__sandbox-exec", "--read-only", "--", "sh", "-c"])
        .arg("cat existing.txt && ls >/dev/null && echo x > created.txt")
        .current_dir(&base)
        .output()
        .expect("spawn sandboxed shell");
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

    if !mermaid_runtime::read_only_containment_available() {
        eprintln!("skipping: kernel cannot enforce read-only containment");
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
