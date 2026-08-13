//! Integration test for `mermaid storage` CLI subcommands (reconcile, gc, delete).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn sandbox_dir() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "mermaid-storage-test-{}-{}",
        std::process::id(),
        nonce
    ));
    fs::create_dir_all(&dir).expect("create sandbox dir");
    dir
}

fn run_sandboxed(home: &Path, extra_args: &[&str]) -> std::process::Output {
    let work = home.join("work");
    fs::create_dir_all(&work).expect("create workdir");
    Command::new(env!("CARGO_BIN_EXE_mermaid"))
        .args(extra_args)
        .current_dir(&work)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("MERMAID_CONFIG_DIR", home.join("config").join("mermaid"))
        .env("MERMAID_DATA_DIR", home.join("data").join("mermaid"))
        .output()
        .expect("spawn mermaid")
}

#[test]
fn storage_reconcile_and_gc_run_successfully() {
    let home = sandbox_dir();
    let work = home.join("work");
    let conv_dir = work.join(".mermaid").join("conversations");
    fs::create_dir_all(&conv_dir).expect("create conv dir");

    // Create a dummy conversation
    let conv_file = conv_dir.join("20260810_120000_000.json");
    fs::write(
        &conv_file,
        r#"{"id":"20260810_120000_000","title":"Test Session","model_name":"gpt-4","total_tokens":50}"#,
    )
    .expect("write conv file");

    // Run `mermaid storage reconcile`
    let output = run_sandboxed(&home, &["storage", "reconcile"]);
    assert!(
        output.status.success(),
        "storage reconcile must exit 0: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Storage reconciliation report"), "{stdout}");
    assert!(stdout.contains("Backfilled SQLite sessions: 1"), "{stdout}");

    // Run `mermaid storage gc`
    let gc_output = run_sandboxed(&home, &["storage", "gc"]);
    assert!(
        gc_output.status.success(),
        "storage gc must exit 0: {}",
        String::from_utf8_lossy(&gc_output.stderr)
    );
    let gc_stdout = String::from_utf8_lossy(&gc_output.stdout);
    assert!(
        gc_stdout.contains("Storage garbage collection complete"),
        "{gc_stdout}"
    );

    // Run `mermaid storage delete 20260810_120000_000`
    let del_output = run_sandboxed(&home, &["storage", "delete", "20260810_120000_000"]);
    assert!(
        del_output.status.success(),
        "storage delete must exit 0: {}",
        String::from_utf8_lossy(&del_output.stderr)
    );
    assert!(!conv_file.exists(), "conversation file must be deleted");

    let _ = fs::remove_dir_all(&home);
}
