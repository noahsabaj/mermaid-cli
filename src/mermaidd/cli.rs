//! The daemon's command line: `--help`, `--version`, and nothing else.

pub const HELP: &str = "\
mermaidd — Mermaid's background daemon (durable runtime state, remote attach,
long-running process ownership).

Usage: mermaidd [--version | --help]

With no arguments it runs the daemon in the foreground, serving the control
socket (a Unix domain socket; an owner-locked named pipe on Windows). On Linux
it is normally managed via the `mermaid daemon` subcommands (install, start,
stop, restart, status, logs) rather than invoked directly.
";

#[derive(Debug, PartialEq, Eq)]
pub enum CliAction {
    Run,
    Version,
    Help,
    Unknown(String),
}

/// Classify `mermaidd`'s invocation. The daemon takes no real arguments, but it
/// is on `PATH` (and in the distro packages), so it must answer
/// `--version`/`--help` and reject unknown arguments rather than silently
/// starting the daemon: a `mermaidd --version` probe would otherwise boot a
/// foreground daemon and — because startup replaces the control socket — could
/// knock a running daemon off its socket.
pub fn classify_args<I: IntoIterator<Item = String>>(args: I) -> CliAction {
    match args.into_iter().next().as_deref() {
        None => CliAction::Run,
        Some("--version" | "-V" | "version") => CliAction::Version,
        Some("--help" | "-h" | "help") => CliAction::Help,
        Some(other) => CliAction::Unknown(other.to_string()),
    }
}
