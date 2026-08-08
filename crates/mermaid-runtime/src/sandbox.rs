//! Optional OS sandboxing for model-driven shell commands.
//!
//! Two independent confinement dimensions, one platform-neutral facade:
//!
//! - A **network kill-switch** (`--no-network` / `safety.network = "deny"`).
//!   Linux: a seccomp-BPF filter — creating an internet socket (`AF_INET` /
//!   `AF_INET6`) dies with `SIGSYS`, while `AF_UNIX` and other local socket
//!   domains keep working so nscd / D-Bus / X11 are unaffected. `SIGSYS` is a
//!   distinctive, catchable signal, which the exec tool maps to a clear
//!   "blocked by the network sandbox" outcome. macOS: a Seatbelt
//!   `(deny network*)` sparing `AF_UNIX`, denied at use time with `EPERM`
//!   (no signal — detection is a hedged text signature).
//! - A **filesystem write-confinement** (`--confine-fs` /
//!   `safety.filesystem = "project"`). Write-class access (create / write /
//!   truncate / remove / rename) is allowed only beneath an explicit set of
//!   directories; everything else fails with a permission error. Reads and
//!   execution stay unrestricted. Linux: Landlock, best-effort by design — a
//!   kernel without it (pre-5.13) degrades to a warned no-op rather than
//!   refusing to run. macOS: Seatbelt `deny file-write*` outside the roots.
//!
//! Everything funnels through [`enforce`], called from the
//! `mermaid __sandbox-exec` launcher — ordinary, single-threaded code — just
//! before it runs the real command. On Linux the restrictions are applied to
//! the launcher itself ([`Enforcement::SelfApplied`]) and survive `execve` and
//! `fork`; on macOS the argv is rewritten to run under `/usr/bin/sandbox-exec`
//! ([`Enforcement::ExecArgv`]), whose profile is likewise inherited by
//! everything the command spawns. Platforms without a backend return `Err`
//! when confinement was requested, so the launcher fails closed (exit 126)
//! instead of ever running the command unconfined.

use std::ffi::OsString;
use std::path::PathBuf;

/// What the caller asked the OS to confine. Both dimensions are independent;
/// an all-off policy enforces nothing (and [`enforce`] is a no-op for it on
/// every platform).
#[derive(Debug, Clone, Default)]
pub struct SandboxPolicy {
    /// Deny network access (internet sockets; local `AF_UNIX` is spared).
    pub deny_network: bool,
    /// When non-empty, confine write-class filesystem access to (beneath)
    /// these directories. Roots that don't exist are skipped, narrowing the
    /// sandbox rather than erroring.
    pub allowed_writes: Vec<PathBuf>,
}

/// How the platform enforced a [`SandboxPolicy`] — the contract between
/// [`enforce`] and the `__sandbox-exec` launcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Enforcement {
    /// The restrictions were installed on the *current* process (Linux
    /// seccomp + Landlock); the caller must now exec its own argv.
    /// `fs_enforced` is `false` when write-confinement was requested but the
    /// kernel cannot enforce it (documented best-effort — the caller warns
    /// and continues).
    SelfApplied { fs_enforced: bool },
    /// The caller must exec this rewritten argv instead (macOS: the command
    /// wrapped in `/usr/bin/sandbox-exec`).
    ExecArgv(Vec<OsString>),
    /// The platform ran the child itself and this is its exit code
    /// (Windows AppContainer follow-up; no constructor yet).
    Ran(i32),
}

/// Enforce `policy` for the command `argv`. Called from single-threaded
/// launcher code. Any `Err` means the requested confinement could not be
/// applied — the caller MUST fail closed (exit 126), never run unconfined.
///
/// # Errors
///
/// The platform refusing to install a *requested* restriction: on Linux a
/// Landlock or seccomp setup failure, on macOS a profile that
/// `sandbox-exec` will not take. An empty policy is `Ok` everywhere, and a
/// kernel too old for write-confinement is `Ok` with `fs_enforced: false`
/// rather than an error — that one case is the documented best-effort, and it
/// is why `Ok` alone is not proof the filesystem is confined.
pub fn enforce(policy: &SandboxPolicy, argv: &[OsString]) -> anyhow::Result<Enforcement> {
    if !policy.deny_network && policy.allowed_writes.is_empty() {
        // Nothing requested: nothing to enforce, on any platform.
        return Ok(Enforcement::SelfApplied { fs_enforced: true });
    }
    #[cfg(target_os = "linux")]
    {
        use anyhow::Context as _;
        // Self-applied: the caller execs its own argv afterwards.
        let _ = argv;
        // Landlock first (its setup opens the allowed dirs), then seccomp.
        let mut fs_enforced = true;
        if !policy.allowed_writes.is_empty() {
            fs_enforced = linux::apply_fs_confinement(&policy.allowed_writes)
                .context("filesystem sandbox unavailable")?;
        }
        if policy.deny_network {
            linux::apply_network_killswitch().context("network sandbox unavailable")?;
        }
        Ok(Enforcement::SelfApplied { fs_enforced })
    }
    #[cfg(target_os = "macos")]
    {
        anyhow::ensure!(
            macos::sandbox_exec_present(),
            "{} not found; refusing to run the command unconfined",
            macos::SANDBOX_EXEC
        );
        Ok(Enforcement::ExecArgv(macos::wrap_argv(policy, argv)))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = argv;
        anyhow::bail!("no OS sandbox backend on this platform; refusing to run unconfined")
    }
}

/// Whether the network kill-switch is really available on this platform:
/// Linux when the seccomp filter assembles (supported arch), macOS when
/// `/usr/bin/sandbox-exec` exists, `false` everywhere else (Windows
/// AppContainer is a follow-up). Used by `mermaid self-test` and the exec
/// tool as a safe, fork-free probe — it installs nothing.
#[must_use]
pub fn network_killswitch_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        linux::network_filter().is_ok()
    }
    #[cfg(target_os = "macos")]
    {
        macos::sandbox_exec_present()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        false
    }
}

/// Whether filesystem write-confinement is really available on this platform:
/// Linux when the Landlock ruleset assembles, macOS when
/// `/usr/bin/sandbox-exec` exists, `false` everywhere else. Like
/// [`network_killswitch_available`]: a safe probe that restricts nothing.
/// (On Linux enforcement remains best-effort at apply time — a pre-Landlock
/// kernel builds the ruleset but cannot enforce it.)
#[must_use]
pub fn fs_confinement_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        linux::fs_ruleset_builds()
    }
    #[cfg(target_os = "macos")]
    {
        macos::sandbox_exec_present()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        false
    }
}

/// macOS Seatbelt backend: generate an allow-default SBPL profile and rewrite
/// the command argv to run under `/usr/bin/sandbox-exec`.
///
/// Profile generation and argv rewriting are PURE functions, compiled (and
/// unit-tested) on every platform — only [`enforce`] reaches them at runtime,
/// and only on macOS. Security invariant: no caller-controlled path is ever
/// spliced into the profile string; paths travel exclusively through `-D`
/// parameters that the profile references as `(param "WRn")`, so a hostile
/// directory name cannot inject SBPL.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod macos {
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    use super::SandboxPolicy;

    /// Fixed, absolute path — deliberately not resolved via `PATH`.
    pub(super) const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

    pub(super) fn sandbox_exec_present() -> bool {
        Path::new(SANDBOX_EXEC).exists()
    }

    /// Build the SBPL profile for `policy`, plus the `-D` parameters it
    /// references (name → path). Allow-default so toolchains keep working;
    /// only the requested dimensions are denied.
    ///
    /// Each existing allowed root yields a literal param (`WRn`) and, when it
    /// differs, a canonicalized one (`WRnC`): on macOS `TMPDIR` lives under
    /// the `/var` firmlink while Seatbelt sees `/private/var`, so matching on
    /// only one form would deny legitimate temp writes (or under-match).
    /// Nonexistent roots are skipped — parity with Landlock's
    /// `path_beneath_rules`, which silently narrows the sandbox for paths it
    /// cannot open.
    pub(super) fn profile(policy: &SandboxPolicy) -> (String, Vec<(String, PathBuf)>) {
        let mut sbpl = String::from("(version 1)\n(allow default)\n");
        if policy.deny_network {
            // Deny all network, then re-allow AF_UNIX sockets (later SBPL
            // rules win) — parity with the Linux seccomp filter, which only
            // kills AF_INET/AF_INET6 so D-Bus/nscd-style local IPC survives.
            // If macOS ever rejects this filter grammar, the CI
            // profile-compile canary fails loudly; the fallback is deleting
            // the two `allow` lines (stricter than Linux, documented delta).
            sbpl.push_str("(deny network*)\n");
            sbpl.push_str("(allow network* (remote unix))\n");
            sbpl.push_str("(allow network* (local unix))\n");
        }
        let mut params: Vec<(String, PathBuf)> = Vec::new();
        if !policy.allowed_writes.is_empty() {
            for (n, root) in policy.allowed_writes.iter().enumerate() {
                if !root.exists() {
                    continue;
                }
                params.push((format!("WR{n}"), root.clone()));
                if let Ok(canonical) = std::fs::canonicalize(root)
                    && canonical != *root
                {
                    params.push((format!("WR{n}C"), canonical));
                }
            }
            if params.is_empty() {
                // Confinement was requested but every allowed root is
                // missing: deny all writes (Landlock parity — missing dirs
                // narrow the sandbox) rather than emit an empty require-all.
                sbpl.push_str("(deny file-write*)\n");
            } else {
                // FOOTGUN: the require-nots MUST be wrapped in `require-all`.
                // A bare filter list ORs its members, and "not under root A
                // OR not under root B" is true for every path once there are
                // two roots — denying ALL writes. `require-all` makes it
                // "outside every allowed root", which is the intent.
                sbpl.push_str("(deny file-write* (require-all");
                for (name, _) in &params {
                    // Param NAMES are generated here (WRn/WRnC, never from
                    // input); path VALUES ride the -D argv, not this string.
                    sbpl.push_str(&format!(" (require-not (subpath (param \"{name}\")))"));
                }
                sbpl.push_str("))\n");
            }
        }
        (sbpl, params)
    }

    /// Rewrite `argv` to run under `sandbox-exec` with the policy's profile:
    /// `/usr/bin/sandbox-exec -p <profile> [-D WRn=<path>]... -- <argv...>`.
    pub(super) fn wrap_argv(policy: &SandboxPolicy, argv: &[OsString]) -> Vec<OsString> {
        let (sbpl, params) = profile(policy);
        let mut wrapped: Vec<OsString> = vec![SANDBOX_EXEC.into(), "-p".into(), sbpl.into()];
        for (name, value) in params {
            wrapped.push("-D".into());
            let mut kv = OsString::from(format!("{name}="));
            kv.push(value.as_os_str());
            wrapped.push(kv);
        }
        // `--` ends option parsing so a command starting with `-` cannot be
        // taken for a sandbox-exec flag.
        wrapped.push("--".into());
        wrapped.extend(argv.iter().cloned());
        wrapped
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn policy(deny_network: bool, allowed_writes: &[PathBuf]) -> SandboxPolicy {
            SandboxPolicy {
                deny_network,
                allowed_writes: allowed_writes.to_vec(),
            }
        }

        /// A fresh existing directory under the temp dir.
        fn tempdir(tag: &str) -> PathBuf {
            let dir = std::env::temp_dir().join(format!(
                "mermaid-sbpl-test-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        #[test]
        fn network_denial_lines_present_iff_requested() {
            let (with_net, _) = profile(&policy(true, &[]));
            assert!(with_net.contains("(deny network*)"));
            // AF_UNIX-sparing allows ship with the deny (Linux parity).
            assert!(with_net.contains("(allow network* (remote unix))"));
            assert!(with_net.contains("(allow network* (local unix))"));
            // deny_network-only: no write confinement may leak into the profile.
            assert!(!with_net.contains("file-write"));

            let dir = tempdir("no-net");
            let (without_net, _) = profile(&policy(false, std::slice::from_ref(&dir)));
            assert!(!without_net.contains("network"));
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn write_denial_nests_require_nots_under_require_all() {
            let a = tempdir("ra-a");
            let b = tempdir("ra-b");
            let (sbpl, params) = profile(&policy(false, &[a.clone(), b.clone()]));
            // Both roots exist and are already canonical-ish; at least the
            // two literal params must be present and referenced.
            assert!(params.iter().any(|(n, _)| n == "WR0"));
            assert!(params.iter().any(|(n, _)| n == "WR1"));
            // The require-nots are wrapped in ONE require-all (a bare list
            // would OR and deny everything — see the generator comment).
            let deny = sbpl
                .lines()
                .find(|l| l.starts_with("(deny file-write*"))
                .expect("deny file-write* line");
            assert!(deny.starts_with("(deny file-write* (require-all (require-not"));
            assert_eq!(
                deny.matches("(require-not (subpath (param ").count(),
                params.len(),
                "one require-not per emitted param: {deny}"
            );
            let _ = std::fs::remove_dir_all(&a);
            let _ = std::fs::remove_dir_all(&b);
        }

        #[test]
        fn dual_literal_and_canonical_params_when_they_differ() {
            let real = tempdir("canon");
            let sub = real.join("sub");
            std::fs::create_dir_all(&sub).unwrap();
            // `<real>/sub/..` exists but canonicalizes to `<real>` — a
            // platform-neutral stand-in for the macOS TMPDIR firmlink
            // (/var/... vs /private/var/...).
            let alias = sub.join("..");
            let (sbpl, params) = profile(&policy(false, std::slice::from_ref(&alias)));
            let literal = params.iter().find(|(n, _)| n == "WR0").expect("literal");
            let canonical = params.iter().find(|(n, _)| n == "WR0C").expect("canonical");
            assert_eq!(literal.1, alias);
            assert_ne!(canonical.1, alias);
            assert!(sbpl.contains("(param \"WR0\")"));
            assert!(sbpl.contains("(param \"WR0C\")"));
            let _ = std::fs::remove_dir_all(&real);
        }

        #[test]
        fn nonexistent_roots_are_skipped_and_all_missing_denies_all_writes() {
            let missing = std::env::temp_dir().join("mermaid-sbpl-test-definitely-missing");
            let (sbpl, params) = profile(&policy(false, std::slice::from_ref(&missing)));
            assert!(params.is_empty());
            // No surviving root: plain deny (narrowed sandbox, Landlock
            // parity), not an empty require-all of unknown SBPL validity.
            assert!(sbpl.contains("(deny file-write*)\n"));
            assert!(!sbpl.contains("require-all"));
        }

        #[test]
        fn paths_never_reach_the_profile_string() {
            // A directory name full of SBPL metacharacters must not appear in
            // the profile text — it rides only in the -D parameter values.
            let evil = tempdir("evil").join("x) (allow default) (deny");
            std::fs::create_dir_all(&evil).unwrap();
            let (sbpl, params) = profile(&policy(true, std::slice::from_ref(&evil)));
            assert!(!sbpl.contains("allow default) (deny"));
            assert!(
                params.iter().any(|(_, v)| *v == evil),
                "the path must ride a param instead"
            );
            let _ = std::fs::remove_dir_all(evil.parent().unwrap());
        }

        #[test]
        fn wrap_argv_shape_is_frozen() {
            let dir = tempdir("argv");
            let argv: Vec<OsString> = vec!["sh".into(), "-c".into(), "echo hi".into()];
            let wrapped = wrap_argv(&policy(true, std::slice::from_ref(&dir)), &argv);
            assert_eq!(wrapped[0], OsString::from(SANDBOX_EXEC));
            assert_eq!(wrapped[1], OsString::from("-p"));
            let profile_arg = wrapped[2].to_string_lossy();
            assert!(profile_arg.starts_with("(version 1)\n(allow default)\n"));
            assert_eq!(wrapped[3], OsString::from("-D"));
            let kv = wrapped[4].to_string_lossy();
            assert!(kv.starts_with("WR0="), "param assignment: {kv}");
            // `--` separates sandbox-exec options from the wrapped command.
            let sep = wrapped
                .iter()
                .position(|a| a == "--")
                .expect("-- separator");
            assert_eq!(&wrapped[sep + 1..], argv.as_slice());
            let _ = std::fs::remove_dir_all(&dir);
        }
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
