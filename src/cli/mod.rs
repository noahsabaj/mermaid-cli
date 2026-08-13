/// CLI argument parsing and command handling - Gateway
mod args;
mod commands;
mod daemon;
mod feedback;

pub use args::{
    Cli, Commands, DaemonCommand, GitHost, OutputFormat, PairCommand, PluginCommand, PrCommand,
    QaCommand, StorageCommand, resolve_run_prompt,
};
pub use commands::{handle_command, list_models, show_version};
