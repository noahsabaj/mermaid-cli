use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "mermaid")]
#[command(version = "0.1.0")]
#[command(about = "An open-source, model-agnostic AI pair programmer", long_about = None)]
pub struct Cli {
    /// Model to use (e.g., ollama/codellama, openai/gpt-4)
    #[arg(short, long)]
    pub model: Option<String>,

    /// Project directory (defaults to current directory)
    #[arg(short, long)]
    pub path: Option<PathBuf>,

    /// Verbose output
    #[arg(short, long)]
    pub verbose: bool,

    /// Show session picker to choose a previous conversation
    #[arg(long, conflicts_with = "new")]
    pub sessions: bool,

    /// Start a fresh conversation (don't auto-resume)
    #[arg(long, conflicts_with = "sessions")]
    pub new: bool,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Initialize configuration
    Init,
    /// List available models
    List,
    /// Start a chat session (default)
    Chat,
    /// Show version information
    Version,
    /// Check status of dependencies and backends
    Status,
    /// Run a single prompt non-interactively
    Run {
        /// The prompt to execute
        prompt: String,

        /// Output format (text, json, markdown)
        #[arg(short, long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,

        /// Maximum tokens to generate
        #[arg(long)]
        max_tokens: Option<usize>,

        /// Don't execute agent actions (dry run)
        #[arg(long)]
        no_execute: bool,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum OutputFormat {
    /// Plain text output
    Text,
    /// JSON structured output
    Json,
    /// Markdown formatted output
    Markdown,
}
