//! The hidden `mermaid __sandbox-exec` re-exec launcher.
//!
//! The exec tool spawns `mermaid __sandbox-exec [flags] -- <program> <args…>`
//! instead of running a command directly when OS confinement is requested. This
//! process asks the platform backend ([`crate::runtime::enforce`]) to enforce
//! the requested sandbox — network denial (`--no-network`) and/or filesystem
//! write-confinement (repeatable `--confine-writes <dir>`) — from ordinary
//! single-threaded code, then runs the real command. On Linux the seccomp /
//! Landlock restrictions are installed on this process and survive `execve`;
//! on macOS the command is exec'd under `/usr/bin/sandbox-exec` instead. Either
//! way the command and everything it spawns inherit the restriction, and any
//! enforcement failure exits 126 — never an unconfined run.
//!
//! Applying from a fresh process (rather than a `pre_exec` closure in the
//! parent) side-steps the async-signal-safety hazard of the post-`fork`
//! context, and makes the whole path directly testable
//! (`mermaid __sandbox-exec --no-network -- sh -c 'curl …'`).

use std::ffi::OsString;
use std::path::PathBuf;

/// Marker subcommand that routes an invocation to this launcher.
pub const SANDBOX_EXEC_SUBCOMMAND: &str = "__sandbox-exec";

/// If `args` (from [`std::env::args_os`]) is a `__sandbox-exec` invocation,
/// apply the requested confinement and `execve` the wrapped command — returning
/// `Some(exit_code)` only if we could not (a failure; on success `execve`
/// replaces the process and this never returns). Returns `None` for any other
/// invocation so the normal CLI proceeds.
///
/// Must run before the async runtime spawns worker threads, so the seccomp
/// filter applies to a single-threaded image.
pub fn maybe_dispatch<I: IntoIterator<Item = OsString>>(args: I) -> Option<i32> {
    let mut it = args.into_iter();
    let _exe = it.next();
    if it.next()? != SANDBOX_EXEC_SUBCOMMAND {
        return None;
    }

    let mut no_network = false;
    let mut confine_writes: Vec<PathBuf> = Vec::new();
    let mut argv: Vec<OsString> = Vec::new();
    let mut in_argv = false;
    while let Some(arg) = it.next() {
        if in_argv {
            argv.push(arg);
        } else if arg == "--" {
            in_argv = true;
        } else if arg == "--no-network" {
            no_network = true;
        } else if arg == "--confine-writes" {
            match it.next() {
                Some(dir) => confine_writes.push(PathBuf::from(dir)),
                None => {
                    eprintln!(
                        "mermaid {SANDBOX_EXEC_SUBCOMMAND}: --confine-writes needs a directory"
                    );
                    return Some(2);
                },
            }
        } else {
            eprintln!("mermaid {SANDBOX_EXEC_SUBCOMMAND}: unexpected argument {arg:?}");
            return Some(2);
        }
    }

    if argv.is_empty() {
        eprintln!("mermaid {SANDBOX_EXEC_SUBCOMMAND}: missing command after `--`");
        return Some(2);
    }

    // Platform enforcement lives behind one facade (Linux: seccomp/Landlock
    // self-applied here; macOS: argv rewritten onto sandbox-exec). Fail
    // closed: if the caller asked for confinement and the platform cannot
    // apply it, exit 126 — never run the command unconfined.
    let policy = crate::runtime::SandboxPolicy {
        deny_network: no_network,
        allowed_writes: confine_writes,
    };
    match crate::runtime::enforce(&policy, &argv) {
        Ok(crate::runtime::Enforcement::SelfApplied { fs_enforced }) => {
            // A kernel that simply can't enforce Landlock degrades to a
            // warned no-op (documented best-effort — pre-5.13 kernels).
            if !fs_enforced {
                eprintln!(
                    "mermaid {SANDBOX_EXEC_SUBCOMMAND}: filesystem confinement not enforced (kernel without Landlock); continuing"
                );
            }
            Some(exec_wrapped(&argv))
        },
        Ok(crate::runtime::Enforcement::ExecArgv(wrapped)) => Some(exec_wrapped(&wrapped)),
        Ok(crate::runtime::Enforcement::Ran(code)) => Some(code),
        Err(err) => {
            eprintln!("mermaid {SANDBOX_EXEC_SUBCOMMAND}: sandbox unavailable: {err:#}");
            Some(126)
        },
    }
}

/// Replace the current process image with `argv`. On unix this `execve`s (never
/// returns on success); elsewhere it spawns and forwards the exit code.
fn exec_wrapped(argv: &[OsString]) -> i32 {
    let program = &argv[0];
    let rest = &argv[1..];

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // `.exec()` only returns on failure.
        let err = std::process::Command::new(program).args(rest).exec();
        eprintln!("mermaid {SANDBOX_EXEC_SUBCOMMAND}: failed to exec {program:?}: {err}");
        127
    }
    #[cfg(not(unix))]
    {
        match std::process::Command::new(program).args(rest).status() {
            Ok(status) => status.code().unwrap_or(1),
            Err(err) => {
                eprintln!("mermaid {SANDBOX_EXEC_SUBCOMMAND}: failed to spawn {program:?}: {err}");
                127
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(items: &[&str]) -> Vec<OsString> {
        items.iter().map(OsString::from).collect()
    }

    #[test]
    fn non_sandbox_invocation_is_ignored() {
        assert_eq!(maybe_dispatch(os(&["mermaid", "run", "hi"])), None);
        assert_eq!(maybe_dispatch(os(&["mermaid"])), None);
    }

    #[test]
    fn missing_command_after_separator_errors() {
        assert_eq!(
            maybe_dispatch(os(&["mermaid", "__sandbox-exec", "--no-network", "--"])),
            Some(2)
        );
    }

    #[test]
    fn unknown_flag_before_separator_errors() {
        assert_eq!(
            maybe_dispatch(os(&["mermaid", "__sandbox-exec", "--bogus", "--", "true"])),
            Some(2)
        );
    }

    #[test]
    fn confine_writes_without_a_directory_errors() {
        assert_eq!(
            maybe_dispatch(os(&["mermaid", "__sandbox-exec", "--confine-writes"])),
            Some(2)
        );
        // A trailing `--confine-writes` must not swallow the `--` separator as
        // its value and then exec with an empty argv.
        assert_eq!(
            maybe_dispatch(os(&["mermaid", "__sandbox-exec", "--confine-writes", "--"])),
            Some(2)
        );
    }
}
