/// CLI argument parsing and command handling - Gateway
mod args;
mod commands;
mod daemon;

pub use args::{Cli, Commands, DaemonCommand, OutputFormat, PluginCommand, QaCommand};
pub use commands::{handle_command, list_models, show_version};
