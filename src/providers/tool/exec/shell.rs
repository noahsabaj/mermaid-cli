//! Shell invocation and working-directory containment: which interpreter, and
//! whether the command is provably confined to the scratchpad.
use super::*;

/// Build the shell `Command` for a model command, optionally wrapped in the
/// `__sandbox-exec` launcher for the network kill-switch and/or filesystem
/// write-confinement (platform backend chosen by the launcher). The caller
/// sets stdio, process group, cwd, and env scrubbing on the returned command.
/// The resolved program + argv for a foreground command — one description
/// consumed by BOTH spawn paths (tokio pipes and the Unix PTY), so the PTY
/// child execs the exact same `__sandbox-exec` launcher (seccomp/Landlock
/// unchanged) as the pipe child.
pub(crate) struct ShellInvocation {
    pub(crate) program: PathBuf,
    pub(crate) args: Vec<std::ffi::OsString>,
}

/// The PowerShell executable model commands run under on Windows: PowerShell 7
/// (`pwsh`) when installed, else the always-present Windows PowerShell 5.1.
/// Resolved once — a PATH scan per spawn would be pure waste.
pub(crate) fn powershell_program() -> &'static str {
    static PROGRAM: std::sync::LazyLock<&'static str> = std::sync::LazyLock::new(|| {
        let has_pwsh = std::env::var_os("PATH").is_some_and(|path| {
            std::env::split_paths(&path).any(|dir| dir.join("pwsh.exe").is_file())
        });
        if has_pwsh { "pwsh" } else { "powershell" }
    });
    &PROGRAM
}

/// Wrap a model command for `-Command` so PowerShell behaves like a
/// non-interactive script runner: cmdlet errors terminate instead of limping
/// on, and the process exit code is the last native command's exit code
/// rather than PowerShell's bare 0/1. Same shape GitHub Actions uses for its
/// `powershell`/`pwsh` shells — without the trailer, `cargo build` failing
/// with 101 surfaces as exit 0.
pub(crate) fn powershell_wrap(command: &str) -> String {
    format!(
        "$ErrorActionPreference='Stop'\n{command}\nif ((Test-Path -LiteralPath variable:\\LASTEXITCODE)) {{ exit $LASTEXITCODE }}"
    )
}

pub(crate) fn shell_invocation(
    command: &str,
    sandbox_network: bool,
    confine_writes: Option<&[PathBuf]>,
) -> ShellInvocation {
    if sandbox_network || confine_writes.is_some() {
        // `mermaid __sandbox-exec [--no-network] [--confine-writes <dir>]… --
        // sh -c <command>`: the launcher installs the requested confinement on
        // itself, then execs the shell. Unix-only path — Windows never sets
        // these flags.
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("mermaid"));
        let mut args: Vec<std::ffi::OsString> = vec!["__sandbox-exec".into()];
        if sandbox_network {
            args.push("--no-network".into());
        }
        for dir in confine_writes.unwrap_or_default() {
            args.push("--confine-writes".into());
            args.push(dir.into());
        }
        args.extend(["--".into(), "sh".into(), "-c".into(), command.into()]);
        ShellInvocation { program: exe, args }
    } else if cfg!(target_os = "windows") {
        ShellInvocation {
            program: PathBuf::from(powershell_program()),
            args: vec![
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-Command".into(),
                powershell_wrap(command).into(),
            ],
        }
    } else {
        ShellInvocation {
            program: PathBuf::from("sh"),
            args: vec!["-c".into(), command.into()],
        }
    }
}

pub(crate) fn build_sandboxed_shell(
    command: &str,
    sandbox_network: bool,
    confine_writes: Option<&[PathBuf]>,
) -> Command {
    let invocation = shell_invocation(command, sandbox_network, confine_writes);
    let mut cmd = Command::new(&invocation.program);
    cmd.args(&invocation.args);
    cmd
}

/// Where the effective working directory landed: inside the project, inside
/// the session scratchpad, or outside both (escalated to `ExternalDirectory`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CwdContainment {
    Project,
    Scratchpad,
    External,
}

/// Classify the (already-canonicalized) effective workdir. The scratchpad
/// check canonicalizes the scratch root itself; if that fails (dir missing,
/// permissions) the cwd fails closed to `External` — never to a downgrade.
pub(crate) fn classify_cwd(
    within_project: bool,
    effective_workdir: &Path,
    scratchpad: Option<&Path>,
) -> CwdContainment {
    if within_project {
        return CwdContainment::Project;
    }
    match scratchpad.and_then(|s| std::fs::canonicalize(s).ok()) {
        Some(scratch) if effective_workdir.starts_with(&scratch) => CwdContainment::Scratchpad,
        _ => CwdContainment::External,
    }
}

/// Fail-closed lexical prover: true only when the command, run with its cwd
/// inside the scratchpad, provably cannot touch anything outside it. Any
/// construct we cannot reason about — shell metacharacters, substitutions,
/// expansions, `..`, absolute or embedded paths pointing elsewhere, even a
/// parse failure — fails the proof and the command keeps its normal gating.
/// Over-rejecting is fine here (the command merely prompts as usual);
/// under-rejecting would silently skip an approval.
pub(crate) fn command_provably_in_scratch(command: &str, scratch: &Path) -> bool {
    // Metacharacters make the command opaque to token-level reasoning:
    // separators/pipes can chain arbitrary commands, redirection retargets
    // writes, `$`/backtick substitute or expand unseen text, `~`/globs
    // re-expand at run time, and grouping braces/parens introduce subshells.
    // Checked on the RAW string so even quoted occurrences fail closed.
    const OPAQUE: &[char] = &[
        ';', '|', '&', '<', '>', '$', '`', '~', '*', '?', '[', ']', '(', ')', '{', '}', '!', '\n',
        '\r',
    ];
    if command.contains(OPAQUE) {
        return false;
    }
    let Ok(tokens) = shell_words::split(command) else {
        return false;
    };
    if tokens.is_empty() {
        return false;
    }
    tokens.iter().all(|t| token_provably_in_scratch(t, scratch))
}

/// One token of a scratch-candidate command. Rules, all fail-closed:
/// - `..` anywhere: rejected (can climb out of the scratch cwd).
/// - `:/` anywhere: rejected (URL / remote-host / list-of-paths shapes).
/// - Drive-designator shape (`C:x`, `c:\x`): rejected on every platform —
///   on Windows it targets a drive root or a per-drive cwd, never scratch.
/// - No path separator: fine — a bare word, flag, or PATH-resolved argv0.
/// - Rooted: must sit lexically inside the scratchpad. `has_root`, not
///   `is_absolute` — on Windows `/etc/passwd` is rooted but not "absolute"
///   (no drive prefix), yet still escapes the scratch cwd via the drive
///   root, so every rooted token gets the containment check.
/// - Relative with a separator: accepted only as a PLAIN path (no leading
///   `-`, no `=`) so flag-embedded paths (`-t/etc`, `--output=/etc/x`,
///   `VAR=/etc`) can't smuggle a target past the rooted check.
pub(crate) fn token_provably_in_scratch(token: &str, scratch: &Path) -> bool {
    if token.contains("..") || token.contains(":/") {
        return false;
    }
    let bytes = token.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        return false;
    }
    if !token.contains(['/', '\\']) {
        return true;
    }
    if Path::new(token).has_root() {
        return Path::new(token).starts_with(scratch);
    }
    !token.starts_with('-') && !token.contains('=')
}

/// Advertised to spawned commands so scripts have a ready-made place for
/// throwaway files that never dirties the project tree.
pub(crate) const SCRATCHPAD_ENV_VAR: &str = "MERMAID_SCRATCHPAD";

/// Export the session scratchpad to a child command (pipe + background spawn
/// paths; the PTY path sets the same variable on its `CommandBuilder`). No-op
/// when the session has no scratchpad materialized.
pub(crate) fn export_scratchpad_env(cmd: &mut Command, scratchpad: Option<&Path>) {
    if let Some(dir) = scratchpad {
        cmd.env(SCRATCHPAD_ENV_VAR, dir);
    }
}

pub(crate) fn tail_lines(text: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..].join("\n")
}

pub(crate) fn first_url(text: &str) -> Option<String> {
    text.split_whitespace()
        .find(|part| part.starts_with("http://") || part.starts_with("https://"))
        .map(|url| {
            url.trim_matches(|c: char| matches!(c, ')' | ']' | '}' | ',' | ';' | '"' | '\''))
                .to_string()
        })
}

pub(crate) fn all_urls(text: &str) -> Vec<String> {
    text.split_whitespace()
        .filter(|part| part.starts_with("http://") || part.starts_with("https://"))
        .map(|url| {
            url.trim_matches(|c: char| matches!(c, ')' | ']' | '}' | ',' | ';' | '"' | '\''))
                .to_string()
        })
        .collect()
}

pub(crate) async fn open_browser_url(url: &str) -> Result<(), String> {
    // Only ever hand a plain http(s) URL to the OS launcher — reject
    // `file:`/`javascript:`/`data:`/etc. supplied by the model. On Windows this
    // is also what lets us drop the `cmd` shell below safely.
    super::super::web::require_http_scheme(url)?;

    #[cfg(target_os = "macos")]
    let mut command = {
        let mut cmd = Command::new("open");
        cmd.arg(url);
        cmd
    };

    #[cfg(target_os = "linux")]
    let mut command = {
        let mut cmd = Command::new("xdg-open");
        cmd.arg(url);
        cmd
    };

    #[cfg(target_os = "windows")]
    let mut command = {
        // Launch via `rundll32` (a real executable) rather than `cmd /C start`,
        // so the URL is passed as a single argv and never re-parsed by a shell —
        // `& | > ^ "` in a model-supplied URL can't break out into arbitrary
        // commands the way they can inside `cmd`.
        let mut cmd = Command::new("rundll32");
        cmd.args(["url.dll,FileProtocolHandler", url]);
        cmd
    };

    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(false)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}
