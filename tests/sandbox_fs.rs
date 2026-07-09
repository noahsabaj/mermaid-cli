//! Integration coverage for the `mermaid __sandbox-exec` Landlock filesystem
//! write-confinement, end-to-end through the real binary. Linux-only;
//! `#[ignore]`d so it runs in the dedicated integration CI job rather than the
//! default suite.
//!
//! The Landlock mechanism itself is unit-tested in `mermaid-runtime::sandbox`
//! (fork + write + assert EACCES); this proves the launcher wiring: that
//! `mermaid __sandbox-exec --confine-writes <dir> -- <cmd>` actually restricts
//! the wrapped command.
#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// Whether this kernel has Landlock in its active LSM list. When it doesn't,
/// confinement is documented best-effort no-op and the assertions below would
/// be vacuous — skip instead of failing.
fn landlock_active() -> bool {
    std::fs::read_to_string("/sys/kernel/security/lsm")
        .map(|lsms| lsms.split(',').any(|l| l.trim() == "landlock"))
        .unwrap_or(false)
}

fn fresh_base() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before epoch")
        .as_nanos();
    let base = std::env::temp_dir().join(format!(
        "mermaid-sandbox-fs-{}-{}",
        std::process::id(),
        nonce
    ));
    std::fs::create_dir_all(&base).expect("create test base dir");
    base
}

#[test]
#[ignore = "spawns the real binary; run with: cargo test --test sandbox_fs -- --ignored"]
fn confine_writes_allows_inside_and_denies_outside() {
    if !landlock_active() {
        eprintln!("skipping: kernel has no active landlock LSM");
        return;
    }
    let bin = env!("CARGO_BIN_EXE_mermaid");
    let base = fresh_base();
    let allowed = base.join("allowed");
    let outside = base.join("outside");
    std::fs::create_dir_all(&allowed).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let allowed_arg = allowed.to_str().unwrap().to_string();

    // Inside the allowed dir: the write succeeds.
    let inside = Command::new(bin)
        .args([
            "__sandbox-exec",
            "--confine-writes",
            &allowed_arg,
            "--confine-writes",
            "/dev",
            "--",
            "sh",
            "-c",
            &format!("echo hi > {}/in.txt", allowed.display()),
        ])
        .output()
        .expect("spawn confined shell (inside)");
    assert!(
        inside.status.success(),
        "write inside the allowed dir should succeed (stderr={})",
        String::from_utf8_lossy(&inside.stderr)
    );
    assert!(allowed.join("in.txt").exists());

    // Outside it: the write is denied with a permission error.
    let out_file = outside.join("out.txt");
    let denied = Command::new(bin)
        .args([
            "__sandbox-exec",
            "--confine-writes",
            &allowed_arg,
            "--confine-writes",
            "/dev",
            "--",
            "sh",
            "-c",
            &format!("echo hi > {}", out_file.display()),
        ])
        .output()
        .expect("spawn confined shell (outside)");
    assert!(
        !denied.status.success(),
        "write outside the allowed dir should fail"
    );
    assert!(!out_file.exists(), "denied write must not create the file");

    let _ = std::fs::remove_dir_all(&base);
}
