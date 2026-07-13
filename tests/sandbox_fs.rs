//! Integration coverage for the `mermaid __sandbox-exec` filesystem
//! write-confinement, end-to-end through the real binary. Linux (Landlock) +
//! macOS (Seatbelt); `#[ignore]`d so it runs in the dedicated integration CI
//! jobs rather than the default suite.
//!
//! The Landlock mechanism itself is unit-tested in `mermaid-runtime::sandbox`
//! (fork + write + assert EACCES); this proves the launcher wiring: that
//! `mermaid __sandbox-exec --confine-writes <dir> -- <cmd>` actually restricts
//! the wrapped command.
#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Whether this kernel has Landlock in its active LSM list. When it doesn't,
/// confinement is documented best-effort no-op and the assertions below would
/// be vacuous — skip instead of failing. Linux-only gate: macOS Seatbelt is
/// always enforcing when `sandbox-exec` exists (and the launcher fails closed
/// when it doesn't).
#[cfg(target_os = "linux")]
fn landlock_active() -> bool {
    std::fs::read_to_string("/sys/kernel/security/lsm")
        .map(|lsms| lsms.split(',').any(|l| l.trim() == "landlock"))
        .unwrap_or(false)
}

fn fresh_base() -> PathBuf {
    // (pid, nanos) alone is NOT unique here: libtest runs these tests as
    // threads of one process (same pid) and starts them in the same instant,
    // while the macOS realtime clock ticks in ~1µs steps — so two tests can
    // draw identical nonces, share a base, and one test's remove_dir_all
    // teardown then deletes the other's live base mid-run (the sandboxed
    // child fails with ENOENT). The per-process counter breaks the tie.
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before epoch")
        .as_nanos();
    let base = std::env::temp_dir().join(format!(
        "mermaid-sandbox-fs-{}-{}-{}",
        std::process::id(),
        nonce,
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&base).expect("create test base dir");
    base
}

#[test]
#[ignore = "spawns the real binary; run with: cargo test --test sandbox_fs -- --ignored"]
fn confine_writes_allows_inside_and_denies_outside() {
    #[cfg(target_os = "linux")]
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

/// macOS: `std::env::temp_dir()` is `/var/folders/...` — an UNcanonicalized
/// firmlink alias of `/private/var/folders/...`, which is what Seatbelt
/// actually sees. `fresh_base()` deliberately passes that uncanonicalized
/// path; the profile must emit both literal and canonicalized `subpath`
/// params or this inside-write would be denied.
#[test]
#[cfg(target_os = "macos")]
#[ignore = "spawns the real binary; run with: cargo test --test sandbox_fs -- --ignored"]
fn confine_writes_honors_uncanonicalized_tmpdir_root() {
    let bin = env!("CARGO_BIN_EXE_mermaid");
    let base = fresh_base();
    let base_arg = base.to_str().unwrap().to_string();

    let inside = Command::new(bin)
        .args([
            "__sandbox-exec",
            "--confine-writes",
            &base_arg,
            "--confine-writes",
            "/dev",
            "--",
            "sh",
            "-c",
            &format!("echo hi > {}/in.txt", base.display()),
        ])
        .output()
        .expect("spawn confined shell (tmpdir)");
    assert!(
        inside.status.success(),
        "write inside an uncanonicalized TMPDIR root should succeed (stderr={})",
        String::from_utf8_lossy(&inside.stderr)
    );
    assert!(base.join("in.txt").exists());

    let _ = std::fs::remove_dir_all(&base);
}

/// macOS Tahoe canary: the full profile (network denial including the
/// AF_UNIX-sparing filters, plus write confinement with dual params) must
/// COMPILE under this OS's `sandbox-exec`. If Apple's SBPL grammar rejects
/// anything we generate, sandbox-exec exits non-zero before running the
/// command and this fails loudly in CI.
#[test]
#[cfg(target_os = "macos")]
#[ignore = "spawns the real binary; run with: cargo test --test sandbox_fs -- --ignored"]
fn seatbelt_profile_compiles_under_both_policies() {
    let bin = env!("CARGO_BIN_EXE_mermaid");
    let base = fresh_base();
    let base_arg = base.to_str().unwrap().to_string();

    let output = Command::new(bin)
        .args([
            "__sandbox-exec",
            "--no-network",
            "--confine-writes",
            &base_arg,
            "--",
            "/usr/bin/true",
        ])
        .output()
        .expect("spawn confined /usr/bin/true");
    assert!(
        output.status.success(),
        "the generated SBPL profile failed to compile or apply (status={:?}, stderr={})",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let _ = std::fs::remove_dir_all(&base);
}
