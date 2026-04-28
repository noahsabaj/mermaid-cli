use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

use crate::models::ReasoningLevel;

#[derive(Parser, Debug)]
#[command(name = "mermaid")]
#[command(version)]
#[command(about = "An open-source, model-agnostic AI pair programmer", long_about = None)]
pub struct Cli {
    /// Model to use (e.g., qwen3-coder:30b, ollama/llama3)
    #[arg(short, long)]
    pub model: Option<String>,

    /// Reasoning depth (none, minimal, low, medium, high, max).
    /// Overrides the persisted default for this session; the slash
    /// command `/reasoning <level>` and Alt+T can change it at runtime.
    #[arg(long)]
    pub reasoning: Option<ReasoningLevel>,

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

    /// Append every reducer `Msg` to a JSONL file at this path for
    /// debugging / post-mortem replay. Interactive mode only.
    #[arg(long, value_name = "FILE")]
    pub record: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Initialize configuration
    Init,
    /// List available models
    List,
    /// List model/provider capability records
    Models,
    /// Show static and cached capability info for a model id
    ModelInfo {
        /// Model id, e.g. openai/gpt-5.2
        model: String,
    },
    /// Start a chat session (default)
    Chat,
    /// Show version information
    Version,
    /// Check status of dependencies and backends
    Status,
    /// List durable runtime tasks
    Tasks {
        /// Maximum number of tasks to show
        #[arg(short, long, default_value_t = 20)]
        limit: usize,
    },
    /// Show one durable runtime task and its timeline
    Task {
        /// Task id
        id: String,
    },
    /// List daemon/runtime-managed background processes
    Processes {
        /// Maximum number of processes to show
        #[arg(short, long, default_value_t = 20)]
        limit: usize,
    },
    /// Print a managed process log
    Logs {
        /// Process id from `mermaid processes`
        id: String,
    },
    /// Stop a managed process
    Stop {
        /// Process id from `mermaid processes`
        id: String,
    },
    /// Restart a managed process
    Restart {
        /// Process id from `mermaid processes`
        id: String,
    },
    /// Open a URL, file, or managed process URL
    Open {
        /// URL, path, or process id
        target: String,
    },
    /// Show listening TCP ports
    Ports,
    /// List pending approvals
    Approvals,
    /// Approve a pending approval record
    Approve {
        /// Approval id
        id: String,
    },
    /// Deny a pending approval record
    Deny {
        /// Approval id
        id: String,
    },
    /// List recent persisted tool runs
    ToolRuns {
        /// Maximum number of tool runs to show
        #[arg(short, long, default_value_t = 20)]
        limit: usize,
    },
    /// List checkpoints
    Checkpoints {
        /// Maximum number of checkpoints to show
        #[arg(short, long, default_value_t = 20)]
        limit: usize,
    },
    /// Restore a checkpoint by id
    Restore {
        /// Checkpoint id
        id: String,
    },
    /// List project memory entries
    Memory {
        /// Project path, defaults to current directory
        #[arg(long)]
        project: Option<PathBuf>,
    },
    /// Write a project memory entry
    Remember {
        /// Memory key
        key: String,
        /// Memory value
        value: String,
        /// Project path, defaults to current directory
        #[arg(long)]
        project: Option<PathBuf>,
    },
    /// Soft-delete a memory entry
    Forget {
        /// Memory entry id
        id: String,
    },
    /// Edit an existing memory entry value
    MemoryEdit {
        /// Memory entry id
        id: String,
        /// New memory value
        value: String,
    },
    /// Manage Mermaid plugin bundles
    Plugin {
        #[command(subcommand)]
        command: PluginCommand,
    },
    /// Manage the Mermaid daemon as a Linux systemd user service
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    /// Create a remote pairing token
    Pair {
        /// Human label for the remote client
        #[arg(long)]
        label: Option<String>,
    },
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
    /// Configure Ollama Cloud API key (interactive prompt). Run this
    /// from your shell before starting mermaid — it reads stdin and
    /// doesn't work from inside the TUI.
    CloudSetup,
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

#[derive(Subcommand, Debug)]
pub enum PluginCommand {
    /// Install a plugin from a local path
    Install {
        /// Path containing plugin.toml
        path: PathBuf,
    },
    /// List installed plugins
    List,
    /// Enable an installed plugin
    Enable {
        /// Plugin id or name
        id: String,
    },
    /// Disable an installed plugin
    Disable {
        /// Plugin id or name
        id: String,
    },
    /// Validate a plugin manifest without installing
    Audit {
        /// Path containing plugin.toml
        path: PathBuf,
    },
}

#[derive(Subcommand, Debug)]
pub enum DaemonCommand {
    /// Install the systemd user service for this user
    Install {
        /// Start and enable the service after writing the unit
        #[arg(long)]
        start: bool,
        /// Overwrite an existing Mermaid service unit
        #[arg(long)]
        force: bool,
    },
    /// Remove the systemd user service for this user
    Uninstall,
    /// Start the daemon user service
    Start,
    /// Stop the daemon user service
    Stop,
    /// Restart the daemon user service
    Restart,
    /// Show daemon service status
    Status,
    /// Show daemon service logs
    Logs {
        /// Follow log output
        #[arg(short, long)]
        follow: bool,
        /// Number of log lines to show before following/exiting
        #[arg(short = 'n', long, default_value_t = 100)]
        lines: usize,
    },
    /// Print the generated service unit without installing it
    PrintUnit,
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
