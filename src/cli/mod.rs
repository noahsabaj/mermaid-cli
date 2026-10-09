/// CLI argument parsing and command handling - Gateway
mod args;
mod commands;
mod daemon;
mod feedback;

pub use args::{
    Cli, Commands, DaemonCommand, McpCommand, OutputFormat, PairCommand, PluginCommand, QaCommand,
    resolve_run_prompt,
};
pub use commands::{handle_command, list_models};
