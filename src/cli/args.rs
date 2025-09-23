use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "mermaid")]
#[command(version = "0.1.0")]
#[command(about = "An open-source, model-agnostic AI pair programmer", long_about = None)]
pub struct Cli {
    /// Model to use (e.g., ollama/codellama, openai/gpt-4, anthropic/claude-3)
    #[arg(short, long)]
    pub model: Option<String>,

    /// Path to configuration file
    #[arg(short, long)]
    pub config: Option<PathBuf>,

    /// Verbose output
    #[arg(short, long)]
    pub verbose: bool,

    /// Project directory (defaults to current directory)
    #[arg(short, long)]
    pub path: Option<PathBuf>,

    /// Skip automatic model installation
    #[arg(long)]
    pub no_auto_install: bool,

    /// Don't auto-start LiteLLM proxy
    #[arg(long)]
    pub no_auto_proxy: bool,

    /// Stop LiteLLM proxy on exit
    #[arg(long)]
    pub stop_proxy_on_exit: bool,

    /// Resume a previous conversation in this directory (shows selection UI)
    #[arg(long, conflicts_with = "continue_conversation")]
    pub resume: bool,

    /// Continue the last conversation in this directory
    #[arg(long, name = "continue", conflicts_with = "resume")]
    pub continue_conversation: bool,

    /// Non-interactive prompt to execute
    #[arg(short, long, conflicts_with_all = &["resume", "continue"])]
    pub prompt: Option<String>,

    /// Output format for non-interactive mode
    #[arg(long, value_enum, default_value_t = OutputFormat::Text, requires = "prompt")]
    pub output_format: OutputFormat,

    /// Maximum tokens to generate in response (non-interactive mode)
    #[arg(long, requires = "prompt")]
    pub max_tokens: Option<usize>,

    /// Don't execute agent actions automatically (non-interactive mode)
    #[arg(long, requires = "prompt")]
    pub no_execute: bool,

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
    /// Check status of dependencies
    Status,
}

#[derive(Debug, Clone, ValueEnum)]
pub enum OutputFormat {
    /// Plain text output
    Text,
    /// JSON structured output
    Json,
    /// Markdown formatted output
    Markdown,
}