//! Optional OS sandboxing for model-driven shell commands.
//!
//! Today this is a Linux seccomp-BPF **network kill-switch**. When engaged (via
//! `--no-network` / `safety.network = "deny"`), creating an internet socket
//! (`AF_INET` / `AF_INET6`) is denied with `SIGSYS`, while `AF_UNIX` and other
//! local socket domains keep working so nscd / D-Bus / X11 are unaffected.
//! `SIGSYS` is a distinctive, catchable signal, which the exec tool maps to a
//! clear "blocked by the network sandbox" outcome.
//!
//! The filter is applied from the `mermaid __sandbox-exec` launcher — ordinary,
//! single-threaded code — just before it `execve`s the real command. seccomp
//! filters survive `execve` and `fork`, so the command and everything it spawns
//! inherit the restriction.
//!
//! No-op on non-Linux (macOS Seatbelt / Windows AppContainer are follow-ups),
//! mirroring the platform-gating convention in [`crate::hardening`].

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
    }
}
