//! Integration coverage for the `mermaid __sandbox-exec` filesystem
//! write-confinement, end-to-end through the real binary. Linux (Landlock) +
//! macOS (Seatbelt); `#[ignore]`d so it runs in the dedicated integration CI
//! jobs rather than the default suite.
//!
//! The Landlock mechanism itself is unit-tested in `mermaid-runtime::sandbox`
//! (fork + write + assert EACCES); this proves the launcher wiring: that
//! `mermaid __sandbox-exec --confine-writes <dir> -- <cmd>` actually restricts
//! the wrapped command.
#![cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]

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
#[ignore = "spawns the real binary; run with: cargo test --test integration -- --ignored it::sandbox_fs::"]
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

    let mut inside_cmd = Command::new(bin);
    inside_cmd.args(["__sandbox-exec", "--confine-writes", &allowed_arg]);
    #[cfg(unix)]
    inside_cmd.args(["--confine-writes", "/dev"]);
    inside_cmd.arg("--");

    #[cfg(windows)]
    inside_cmd.args([
        "cmd",
        "/c",
        &format!("echo hi > {}/in.txt", allowed.display()),
    ]);
    #[cfg(not(windows))]
    inside_cmd.args([
        "sh",
        "-c",
        &format!("echo hi > {}/in.txt", allowed.display()),
    ]);

    let inside = inside_cmd.output().expect("spawn confined shell (inside)");
    assert!(
        inside.status.success(),
        "write inside the allowed dir should succeed (stderr={})",
        String::from_utf8_lossy(&inside.stderr)
    );
    assert!(allowed.join("in.txt").exists());

    // Outside it: the write is denied with a permission error.
    let out_file = outside.join("out.txt");
    let mut denied_cmd = Command::new(bin);
    denied_cmd.args(["__sandbox-exec", "--confine-writes", &allowed_arg]);
    #[cfg(unix)]
    denied_cmd.args(["--confine-writes", "/dev"]);
    denied_cmd.arg("--");

    #[cfg(windows)]
    denied_cmd.args(["cmd", "/c", &format!("echo hi > {}", out_file.display())]);
    #[cfg(not(windows))]
    denied_cmd.args(["sh", "-c", &format!("echo hi > {}", out_file.display())]);

    let denied = denied_cmd.output().expect("spawn confined shell (outside)");
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
#[ignore = "spawns the real binary; run with: cargo test --test integration -- --ignored it::sandbox_fs::"]
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
#[ignore = "spawns the real binary; run with: cargo test --test integration -- --ignored it::sandbox_fs::"]
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

/// The plan-mode scratchpad carve-out's enforcement half, end-to-end through
/// the real launcher with the exact roots `SandboxPlan::scratch_confined`
/// builds: the scratchpad plus the discard devices, and nothing else.
///
/// The lexical prover (`is_scratch_only_command`) is the authorization and is
/// unit-tested next to itself. What this proves is the other half — that a
/// command granted the carve-out genuinely cannot reach the project tree even
/// when it names it by absolute path, so the two together are belt and braces
/// rather than one mechanism trusted twice.
#[test]
#[ignore = "spawns the real binary; run with: cargo test --test integration -- --ignored it::sandbox_fs::"]
#[cfg(not(windows))]
fn scratch_confinement_denies_the_project_tree() {
    #[cfg(target_os = "linux")]
    if !landlock_active() {
        eprintln!("skipping: kernel has no active landlock LSM");
        return;
    }
    let bin = env!("CARGO_BIN_EXE_mermaid");
    let base = fresh_base();
    let scratch = base.join("scratchpad");
    let project = base.join("project");
    std::fs::create_dir_all(&scratch).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    let canary = project.join("canary.txt");
    std::fs::write(&canary, "original").unwrap();
    let scratch_arg = scratch.to_str().unwrap().to_string();

    // Exactly the carve-out's roots: scratch + the safe devices. Notably NOT
    // the project root, and NOT the system temp dir that contains both.
    let roots = |cmd: &mut Command| {
        cmd.args(["__sandbox-exec", "--no-network", "--confine-fs"]);
        cmd.args(["--confine-writes", &scratch_arg]);
        for dev in ["/dev/null", "/dev/zero", "/dev/tty"] {
            cmd.args(["--confine-writes", dev]);
        }
        cmd.arg("--");
    };

    let mut inside = Command::new(bin);
    roots(&mut inside);
    inside.args([
        "sh",
        "-c",
        &format!("echo hi > {}/in.txt", scratch.display()),
    ]);
    let inside = inside.output().expect("spawn confined shell (scratch)");
    assert!(
        inside.status.success(),
        "a write into the scratchpad must succeed (stderr={})",
        String::from_utf8_lossy(&inside.stderr)
    );
    assert!(scratch.join("in.txt").exists());

    // The same grant, aimed at the project tree by absolute path.
    let mut denied = Command::new(bin);
    roots(&mut denied);
    denied.args(["sh", "-c", &format!("echo pwned > {}", canary.display())]);
    let denied = denied.output().expect("spawn confined shell (project)");
    assert!(
        !denied.status.success(),
        "a write into the project tree must be denied by the kernel"
    );
    assert_eq!(
        std::fs::read_to_string(&canary).unwrap(),
        "original",
        "the project file must be byte-identical"
    );

    // `2>/dev/null` must still work: the discard devices are in the set.
    let mut discard = Command::new(bin);
    roots(&mut discard);
    discard.args(["sh", "-c", "echo hi 2>/dev/null"]);
    let discard = discard.output().expect("spawn confined shell (discard)");
    assert!(
        discard.status.success(),
        "redirecting to /dev/null must stay possible (stderr={})",
        String::from_utf8_lossy(&discard.stderr)
    );

    let _ = std::fs::remove_dir_all(&base);
}
