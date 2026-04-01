use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "mermaid")]
#[command(version)]
#[command(about = "An open-source, model-agnostic AI pair programmer", long_about = None)]
pub struct Cli {
    /// Model to use (e.g., qwen3-coder:30b, ollama/llama3)
    #[arg(short, long)]
    pub model: Option<String>,

    /// Project directory (defaults to current directory)
    #[arg(short, long)]
    pub path: Option<PathBuf>,

    /// Verbose output
    #[arg(short, long)]
    pub verbose: bool,

    /// Show session picker to choose a previous conversation
    #[arg(long, conflicts_with = "continue_session")]
    pub sessions: bool,

    /// Resume the last conversation instead of starting fresh
    #[arg(long = "continue", conflicts_with = "sessions")]
    pub continue_session: bool,

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
    /// Add an MCP server (e.g., mermaid add context7)
    Add {
        /// MCP server name (e.g., context7, github, filesystem)
        name: String,
    },
    /// Remove a configured MCP server
    Remove {
        /// MCP server name to remove
        name: String,
    },
    /// List configured MCP servers
    Mcp,
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
