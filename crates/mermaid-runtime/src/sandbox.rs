//! Optional OS sandboxing for model-driven shell commands.
//!
//! Two independent Linux confinement dimensions:
//!
//! - A seccomp-BPF **network kill-switch**. When engaged (via `--no-network` /
//!   `safety.network = "deny"`), creating an internet socket (`AF_INET` /
//!   `AF_INET6`) is denied with `SIGSYS`, while `AF_UNIX` and other local
//!   socket domains keep working so nscd / D-Bus / X11 are unaffected. `SIGSYS`
//!   is a distinctive, catchable signal, which the exec tool maps to a clear
//!   "blocked by the network sandbox" outcome.
//! - A Landlock **filesystem write-confinement**. When engaged (via
//!   `--confine-fs` / `safety.filesystem = "project"`), write-class access
//!   (create / write / truncate / remove / rename) is allowed only beneath an
//!   explicit set of directories; everything else fails with `EACCES`. Reads
//!   and execution stay unrestricted. Best-effort by design: a kernel without
//!   Landlock (pre-5.13) degrades to no-op rather than refusing to run.
//!
//! Both are applied from the `mermaid __sandbox-exec` launcher — ordinary,
//! single-threaded code — just before it `execve`s the real command. seccomp
//! filters and Landlock domains survive `execve` and `fork`, so the command and
//! everything it spawns inherit the restriction.
//!
//! No-op on non-Linux (macOS Seatbelt / Windows AppContainer are follow-ups),
//! mirroring the platform-gating convention in [`crate::hardening`].

use std::path::PathBuf;

/// Apply the network kill-switch to the current process. Inherited across
/// `execve`/`fork`. Returns an error if the filter cannot be built or installed,
/// so the caller can fail closed rather than run a command unconfined. A no-op
/// (returning `Ok`) on non-Linux.
pub fn apply_network_killswitch() -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        linux::apply_network_killswitch()
    }
    #[cfg(not(target_os = "linux"))]
    {
        Ok(())
    }
}

/// Whether the network kill-switch can be built on this platform: `true` when
/// the seccomp filter assembles (Linux + a supported arch), or trivially on
/// non-Linux (nothing to enforce). Used by `mermaid self-test` as a safe,
/// fork-free smoke probe — it installs nothing.
pub fn network_killswitch_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        linux::network_filter().is_ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        true
    }
}

/// Confine write-class filesystem access of the current process (and everything
/// it execs/forks) to the given directories via Landlock. Returns `Ok(true)`
/// when the kernel enforces (fully or partially), `Ok(false)` when it cannot
/// (no Landlock — best-effort no-op), and `Err` on a real setup failure so the
/// caller can fail closed. Directories that don't exist are skipped. `Ok(false)`
/// on non-Linux.
pub fn apply_fs_confinement(allowed_writes: &[PathBuf]) -> anyhow::Result<bool> {
    #[cfg(target_os = "linux")]
    {
        linux::apply_fs_confinement(allowed_writes)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = allowed_writes;
        Ok(false)
    }
}

/// Whether the filesystem-confinement ruleset assembles on this platform. Like
/// [`network_killswitch_available`]: a safe, fork-free `self-test` probe that
/// restricts nothing. (Enforcement remains best-effort at apply time — a
/// pre-Landlock kernel builds the ruleset but cannot enforce it.)
pub fn fs_confinement_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        linux::fs_ruleset_builds()
    }
    #[cfg(not(target_os = "linux"))]
    {
        true
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::BTreeMap;

    use anyhow::Context;
    use seccompiler::{
        BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
        SeccompRule, apply_filter,
    };

    /// Build the "deny internet sockets" BPF program: kill the process on
    /// `socket(AF_INET|AF_INET6, …)`, allow everything else (including
    /// `AF_UNIX`). Comparing the low 32 bits (`Dword`) of the domain argument is
    /// deliberate — it catches a family smuggled in the high bits, which the
    /// kernel would still truncate to `AF_INET`.
    pub(super) fn network_filter() -> anyhow::Result<BpfProgram> {
        let inet = SeccompRule::new(vec![SeccompCondition::new(
            0,
            SeccompCmpArgLen::Dword,
            SeccompCmpOp::Eq,
            libc::AF_INET as u64,
        )?])?;
        let inet6 = SeccompRule::new(vec![SeccompCondition::new(
            0,
            SeccompCmpArgLen::Dword,
            SeccompCmpOp::Eq,
            libc::AF_INET6 as u64,
        )?])?;

        let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
        rules.insert(libc::SYS_socket, vec![inet, inet6]);

        let filter = SeccompFilter::new(
            rules,
            SeccompAction::Allow,       // syscalls with no matching rule: allow
            SeccompAction::KillProcess, // an inet socket: SIGSYS-kill the process
            std::env::consts::ARCH
                .try_into()
                .context("seccomp: unsupported target arch")?,
        )
        .context("seccomp: build network filter")?;

        let program: BpfProgram = filter.try_into().context("seccomp: assemble network BPF")?;
        Ok(program)
    }

    pub fn apply_network_killswitch() -> anyhow::Result<()> {
        let program = network_filter()?;
        apply_filter(&program).context("seccomp: install network filter")?;
        Ok(())
    }

    /// Landlock ABI the write-confinement targets. V3 (kernel 6.2) rounds out
    /// the write set with `Truncate` on top of V2's `Refer` (cross-directory
    /// rename/link). `CompatLevel::BestEffort` degrades gracefully on older
    /// kernels.
    const LANDLOCK_ABI: landlock::ABI = landlock::ABI::V3;

    /// Build + apply the "writes only beneath these directories" Landlock
    /// ruleset. Only write-class access is handled, so reads and execution stay
    /// unrestricted everywhere. Returns whether the kernel actually enforces.
    pub(super) fn apply_fs_confinement(
        allowed_writes: &[std::path::PathBuf],
    ) -> anyhow::Result<bool> {
        use landlock::{
            AccessFs, CompatLevel, Compatible, Ruleset, RulesetAttr, RulesetCreatedAttr,
            RulesetStatus, path_beneath_rules,
        };

        let write_access = AccessFs::from_write(LANDLOCK_ABI);
        let status = Ruleset::default()
            .set_compatibility(CompatLevel::BestEffort)
            .handle_access(write_access)
            .context("landlock: handle write access")?
            .create()
            .context("landlock: create ruleset")?
            // `path_beneath_rules` silently skips paths that can't be opened,
            // so a missing allowed dir narrows the sandbox instead of erroring.
            .add_rules(path_beneath_rules(allowed_writes, write_access))
            .context("landlock: add write rules")?
            .restrict_self()
            .context("landlock: restrict self")?;
        Ok(status.ruleset != RulesetStatus::NotEnforced)
    }

    /// Whether the confinement ruleset assembles (fork-free `self-test` probe;
    /// creates a ruleset fd and drops it without restricting anything).
    pub(super) fn fs_ruleset_builds() -> bool {
        use landlock::{AccessFs, CompatLevel, Compatible, Ruleset, RulesetAttr};
        Ruleset::default()
            .set_compatibility(CompatLevel::BestEffort)
            .handle_access(AccessFs::from_write(LANDLOCK_ABI))
            .and_then(|r| r.create())
            .is_ok()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Fork a child, install `bpf`, attempt `socket(domain, SOCK_STREAM, 0)`,
        /// and return the child's raw wait status. The BPF is built in the
        /// parent so the post-`fork` child only performs near-async-signal-safe
        /// work (the seccomp syscall + `socket` + `_exit`).
        fn child_socket_status(bpf: &BpfProgram, domain: libc::c_int) -> libc::c_int {
            // SAFETY: single-threaded test path; the child only calls
            // `apply_filter`, `socket`/`close`, and `_exit`.
            unsafe {
                let pid = libc::fork();
                assert!(pid >= 0, "fork failed");
                if pid == 0 {
                    if apply_filter(bpf).is_err() {
                        libc::_exit(77);
                    }
                    let fd = libc::socket(domain, libc::SOCK_STREAM, 0);
                    if fd >= 0 {
                        libc::close(fd);
                    }
                    libc::_exit(0);
                }
                let mut status: libc::c_int = 0;
                let waited = libc::waitpid(pid, &mut status, 0);
                assert_eq!(waited, pid, "waitpid failed");
                status
            }
        }

        #[test]
        fn inet_socket_is_killed_with_sigsys() {
            let bpf = network_filter().expect("build filter");
            let status = child_socket_status(&bpf, libc::AF_INET);
            assert!(
                libc::WIFSIGNALED(status),
                "AF_INET socket should be signal-killed, status={status}"
            );
            assert_eq!(
                libc::WTERMSIG(status),
                libc::SIGSYS,
                "AF_INET socket should die with SIGSYS"
            );
        }

        #[test]
        fn unix_socket_is_allowed() {
            let bpf = network_filter().expect("build filter");
            let status = child_socket_status(&bpf, libc::AF_UNIX);
            assert!(
                libc::WIFEXITED(status),
                "AF_UNIX socket should exit cleanly, status={status}"
            );
            assert_eq!(
                libc::WEXITSTATUS(status),
                0,
                "AF_UNIX socket must be allowed under the network kill-switch"
            );
        }

        #[test]
        fn fs_confinement_allows_inside_and_denies_outside_writes() {
            // Two sibling temp dirs; confinement grants writes beneath only one.
            let base = std::env::temp_dir().join(format!(
                "mermaid-landlock-test-{}-{}",
                std::process::id(),
                // Distinguish parallel test binaries reusing a pid.
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            let allowed = base.join("allowed");
            let outside = base.join("outside");
            std::fs::create_dir_all(&allowed).unwrap();
            std::fs::create_dir_all(&outside).unwrap();

            // Exit codes: 0 = enforced correctly; 42 = kernel can't enforce
            // (skip); 10 = inside write failed; 11 = outside write succeeded;
            // 77 = apply failed. Same fork pattern as the seccomp tests above.
            // SAFETY: the child only runs the confinement setup, two writes,
            // and `_exit`.
            let status = unsafe {
                let pid = libc::fork();
                assert!(pid >= 0, "fork failed");
                if pid == 0 {
                    let code = match apply_fs_confinement(std::slice::from_ref(&allowed)) {
                        Err(_) => 77,
                        Ok(false) => 42,
                        Ok(true) => {
                            let inside_ok = std::fs::write(allowed.join("in.txt"), b"x").is_ok();
                            let outside_ok = std::fs::write(outside.join("out.txt"), b"x").is_ok();
                            match (inside_ok, outside_ok) {
                                (true, false) => 0,
                                (false, _) => 10,
                                (true, true) => 11,
                            }
                        },
                    };
                    libc::_exit(code);
                }
                let mut status: libc::c_int = 0;
                assert_eq!(libc::waitpid(pid, &mut status, 0), pid, "waitpid failed");
                status
            };

            let _ = std::fs::remove_dir_all(&base);

            assert!(
                libc::WIFEXITED(status),
                "child should exit, status={status}"
            );
            let code = libc::WEXITSTATUS(status);
            if code == 42 {
                eprintln!("skipping: kernel does not enforce Landlock");
                return;
            }
            assert_eq!(
                code, 0,
                "confined child: 10 = allowed write failed, 11 = outside write \
                 succeeded, 77 = apply failed"
            );
        }
    }
}
