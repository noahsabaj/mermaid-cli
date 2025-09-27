/// CLI argument parsing and command handling - Gateway
mod args;
mod commands;

pub use args::{Cli, Commands, OutputFormat};
pub use commands::{handle_command, list_models, show_version};
