//! Recognising a sandbox denial in a child's exit status and output, and
//! building the sandboxed shell invocation.
use super::*;

pub(crate) struct CommandMetadataInput {
    pub(crate) command: String,
    pub(crate) working_dir: Option<String>,
    pub(crate) exit_code: Option<i32>,
    pub(crate) timed_out: bool,
    pub(crate) background: bool,
    pub(crate) stdout_lines: usize,
    pub(crate) stderr_lines: usize,
    pub(crate) detected_urls: Vec<String>,
    pub(crate) pid: Option<u32>,
    pub(crate) log_path: Option<String>,
    pub(crate) byte_count: Option<usize>,
}

pub(crate) fn command_metadata(input: CommandMetadataInput) -> ToolRunMetadata {
    ToolRunMetadata {
        detail: ToolMetadata::ExecuteCommand {
            command: input.command,
            working_dir: input.working_dir,
            exit_code: input.exit_code,
            timed_out: input.timed_out,
            background: input.background,
            stdout_lines: input.stdout_lines,
            stderr_lines: input.stderr_lines,
            detected_urls: input.detected_urls,
            pid: input.pid,
            log_path: input.log_path,
            // Set by the completion arm when a sandbox denial is detected; the
            // metadata builder itself never sees the terminating signal.
            denied_by_sandbox: false,
        },
        line_count: Some(input.stdout_lines + input.stderr_lines),
        byte_count: input.byte_count,
        ..ToolRunMetadata::default()
    }
}

/// The cached OS-sandbox availability probes (network kill-switch, filesystem
/// write-confinement). Probed once per process — platform capability cannot
/// change mid-run, and the Linux probe assembles a BPF program each call.
pub(crate) fn sandbox_probes() -> (bool, bool) {
    static PROBES: std::sync::OnceLock<(bool, bool)> = std::sync::OnceLock::new();
    *PROBES.get_or_init(|| {
        (
            mermaid_runtime::network_killswitch_available(),
            mermaid_runtime::fs_confinement_available(),
        )
    })
}

/// SIGSYS on Linux (`x86_64/aarch64`) — the signal the seccomp kill-switch raises.
pub(crate) const SANDBOX_KILL_SIGNAL: i32 = 31;

/// Which sandbox dimension a completed command's failure matches. `Ambiguous`
/// exists for macOS with both policies active: Seatbelt denies network AND
/// filesystem access with the same `EPERM` text and no signal, so the two
/// cannot be told apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DenialKind {
    Network,
    Filesystem,
    Ambiguous,
}

/// Map a completed run onto a sandbox-denial kind, gated on which policies
/// were actually active for this spawn (so an ordinary permission failure or
/// `exit 159` is never mislabeled when the sandbox was off).
///
/// Signatures per platform:
/// - Linux network: precise — the shell died with SIGSYS or reaped a
///   SIGSYS-killed child (`128 + SIGSYS`). Nothing else produces it.
/// - Linux filesystem: hedged — Landlock denials are ordinary `EACCES` text.
/// - macOS (Seatbelt): both dimensions are hedged `EPERM` "Operation not
///   permitted" text with no signal; with both policies active the match is
///   [`DenialKind::Ambiguous`].
pub(crate) fn detect_denial(
    run: &CommandRunOutput,
    sandbox_network: bool,
    sandbox_fs: bool,
) -> Option<DenialKind> {
    if cfg!(target_os = "linux") {
        if sandbox_network && is_sigsys_denial(run) {
            return Some(DenialKind::Network);
        }
        if sandbox_fs && is_permission_denial(run) {
            return Some(DenialKind::Filesystem);
        }
        return None;
    }
    if !is_permission_denial(run) {
        return None;
    }
    match (sandbox_network, sandbox_fs) {
        (true, true) => Some(DenialKind::Ambiguous),
        (true, false) => Some(DenialKind::Network),
        (false, true) => Some(DenialKind::Filesystem),
        (false, false) => None,
    }
}

/// Message shown when the Linux network kill-switch blocks a command (the
/// precise SIGSYS signature). States the cause and the three ways to allow it.
/// No emojis.
pub(crate) const NETWORK_DENIED_MESSAGE: &str = "Blocked by the network sandbox: this command tried to open an internet socket, which is denied because network access is off (safety.network = \"deny\" / --no-network). Re-run without --no-network, approve the command, or use full-access mode to allow network access.";

/// Hedged network-denial message for platforms without a precise signal
/// (macOS Seatbelt denies with plain EPERM). No emojis.
pub(crate) const HEDGED_NETWORK_DENIED_MESSAGE: &str = "Command failed with a permission error while the network sandbox was active (safety.network = \"deny\" / --no-network); a network access was likely denied. Re-run without --no-network, approve the command, or use full-access mode to allow network access.";

/// Message shown when a command's failure matches the filesystem-sandbox denial
/// signature. Hedged ("likely") because write denials surface as ordinary
/// permission errors (Linux Landlock EACCES, macOS Seatbelt EPERM), unlike the
/// unambiguous SIGSYS of the Linux network kill-switch. No emojis.
pub(crate) const FS_DENIED_MESSAGE: &str = "Command failed with a permission error while the filesystem sandbox was active (safety.filesystem = \"project\" / --confine-fs); a write outside the project directory, the system temp directory, or /dev was likely denied. Write inside the project, or re-run without --confine-fs to allow it.";

/// Combined hedged message for [`DenialKind::Ambiguous`] (macOS, both
/// policies active — the EPERM signature cannot say which one fired). No
/// emojis.
pub(crate) const AMBIGUOUS_DENIED_MESSAGE: &str = "Command failed with a permission error while the network and filesystem sandboxes were active (--no-network / --confine-fs); a network access or a write outside the allowed directories was likely denied. Write inside the project, or re-run without the sandbox flags to allow it.";

/// Whether a completed command was terminated by the Linux seccomp
/// kill-switch: the shell itself died with SIGSYS, or (more often) it reaped a
/// SIGSYS-killed child and exited `128 + SIGSYS`.
pub(crate) fn is_sigsys_denial(run: &CommandRunOutput) -> bool {
    run.signal == Some(SANDBOX_KILL_SIGNAL) || run.exit_code == Some(128 + SANDBOX_KILL_SIGNAL)
}

/// Whether a completed command's failure looks like a sandbox permission
/// denial: non-zero exit plus the shell/tool permission-error text. A
/// signature match, not a proof — [`detect_denial`] gates on "the sandbox was
/// active for this spawn", and the surfaced messages hedge accordingly.
pub(crate) fn is_permission_denial(run: &CommandRunOutput) -> bool {
    let failed = matches!(run.exit_code, Some(code) if code != 0);
    failed
        && (run.output.contains("Permission denied")
            || run.output.contains("Operation not permitted")
            || run.output.contains("Access is denied")
            || run.output.contains("UnauthorizedAccessException")
            || run.output.contains("10013")
            || run.output.contains("WSAEACCES"))
}
