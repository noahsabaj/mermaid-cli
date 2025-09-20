use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "mermaid")]
#[command(version = "0.1.0")]
#[command(about = "🧜‍♀️ An open-source, model-agnostic AI pair programmer", long_about = None)]
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