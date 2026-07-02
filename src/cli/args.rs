use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

use crate::models::ReasoningLevel;

#[derive(Parser, Debug)]
#[command(name = "mermaid")]
#[command(version)]
#[command(about = "An open-source, model-agnostic AI pair programmer", long_about = None)]
#[command(after_help = TOP_LEVEL_HELP_AFTER)]
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

    /// Pick a past conversation to resume from a searchable list (this
    /// directory's sessions). Like `claude --resume`.
    #[arg(long, conflicts_with = "continue_session")]
    pub resume: bool,

    /// Resume the most recent conversation in this directory. Like
    /// `claude --continue`.
    #[arg(long = "continue", conflicts_with = "resume")]
    pub continue_session: bool,

    /// Append every reducer `Msg` to a JSONL file at this path for
    /// debugging / post-mortem replay. Interactive mode only.
    #[arg(long, value_name = "FILE")]
    pub record: Option<PathBuf>,

    /// Replay a `--record` log through the pure reducer: print the
    /// reconstructed session and a determinism verdict. Headless — no model
    /// calls, no tool execution, no config reads (the log is self-contained).
    #[arg(long, value_name = "FILE", conflicts_with = "record")]
    pub replay: Option<PathBuf>,

    /// Replace Mermaid's default system prompt for this invocation
    #[arg(long, global = true, conflicts_with = "system_prompt_file")]
    pub system_prompt: Option<String>,

    /// Replace Mermaid's default system prompt with the contents of a file
    #[arg(
        long,
        value_name = "FILE",
        global = true,
        conflicts_with = "system_prompt"
    )]
    pub system_prompt_file: Option<PathBuf>,

    /// Append extra instructions after Mermaid's system prompt for this invocation
    #[arg(long, global = true)]
    pub append_system_prompt: Option<String>,

    /// Append extra instructions from a file after Mermaid's system prompt
    #[arg(long, value_name = "FILE", global = true)]
    pub append_system_prompt_file: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

const TOP_LEVEL_HELP_AFTER: &str = "\
Common first run:
  mermaid doctor                         Check model, tools, safety, and project readiness
  mermaid                                Start the full-screen terminal coding agent
  mermaid run \"inspect this repo\"        Run one prompt headlessly
  mermaid self-test                      Run fast deterministic Mermaid self-tests

Command groups:
  Everyday: chat, run, doctor, status, list, self-test
  Model/context: models, model-info, --model, --reasoning, --system-prompt*
  Safety/recovery: approvals, approve, deny, checkpoints, restore
  Integrations: add, remove, mcp, cloud-setup, plugin, pr
  Advanced runtime: daemon, tasks, task, processes, logs, stop, restart, ports, pair";

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
        /// Model id, e.g. <provider>/<model>
        model: String,
    },
    /// Start a chat session (default)
    Chat,
    /// Show version information
    Version,
    /// Update Mermaid to the latest release
    Update {
        /// Only report whether an update is available; don't install it
        #[arg(long)]
        check: bool,
        /// Reinstall even if already on the latest version
        #[arg(long)]
        force: bool,
    },
    /// Check status of dependencies and backends
    Status,
    /// Check first-run readiness and explain what Mermaid can do now
    Doctor {
        /// Output format (text, json, markdown)
        #[arg(short, long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,
    },
    /// Run fast deterministic Mermaid self-tests
    SelfTest {
        /// Output format (text, json, markdown)
        #[arg(short, long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,
        /// Keep the temporary self-test workspace after the run
        #[arg(long)]
        keep_workspace: bool,
    },
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
    /// List Mermaid-managed background processes
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
        /// Skip the confirmation prompt (required for non-interactive use)
        #[arg(short, long)]
        force: bool,
    },
    /// Manage Mermaid plugin bundles
    Plugin {
        #[command(subcommand)]
        command: PluginCommand,
    },
    /// Manage Mermaid's Linux background service
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    /// Manage remote pairing tokens
    Pair {
        #[command(subcommand)]
        command: PairCommand,
    },
    /// Internal self-QA commands. Hidden from normal help output.
    #[command(hide = true)]
    Qa {
        #[command(subcommand)]
        command: QaCommand,
    },
    /// Add an MCP server (e.g., mermaid add context7)
    Add {
        /// MCP server name (e.g., context7, git, filesystem)
        name: String,
        /// Skip the confirmation prompt before fetching and running a package
        /// that is not in the built-in registry (for scripted/CI use). Without
        /// this, adding an unknown package fails closed when there is no TTY.
        #[arg(long)]
        yes: bool,
    },
    /// Remove a configured MCP server
    Remove {
        /// MCP server name to remove
        name: String,
    },
    /// List configured MCP servers
    Mcp,
    /// Create a pull/merge request from the current branch via the host CLI
    /// (`gh` for GitHub, `glab` for GitLab)
    Pr {
        #[command(subcommand)]
        command: PrCommand,
    },
    /// Configure Ollama Cloud API key (interactive prompt). Run this
    /// from your shell before starting mermaid — it reads stdin and
    /// doesn't work from inside the TUI.
    CloudSetup,
    /// Run a single prompt non-interactively
    Run {
        /// The prompt to execute
        #[arg(value_parser = non_empty_prompt)]
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

        /// Allow non-replayable tools (web/mcp/subagent/computer-use) to run on
        /// an `Ask` decision in this headless run. Off by default — `ask` mode
        /// otherwise refuses them when there's no approval UI.
        #[arg(long)]
        allow_untrusted_tools: bool,
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
pub enum PairCommand {
    /// Create a pairing token (the secret is printed once)
    Create {
        /// Human label for the remote client
        #[arg(long)]
        label: Option<String>,
        /// Days until the token expires (0 = never expires; default 30)
        #[arg(long)]
        ttl_days: Option<i64>,
    },
    /// List pairing tokens (id, label, created, expiry, status)
    List,
    /// Revoke a pairing token by id
    Revoke {
        /// Pairing token id
        id: String,
    },
}

/// Which Git hosting provider's CLI to drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum GitHost {
    /// GitHub, via the `gh` CLI.
    Github,
    /// GitLab, via the `glab` CLI.
    Gitlab,
}

#[derive(Subcommand, Debug)]
pub enum PrCommand {
    /// Create a PR/MR from the current branch. Wraps `gh pr create` /
    /// `glab mr create`, reusing their existing authentication.
    Create {
        /// PR/MR title. Omitted → filled from the branch's commits.
        #[arg(short, long)]
        title: Option<String>,
        /// PR/MR body text.
        #[arg(short, long)]
        body: Option<String>,
        /// Read the body from a file (e.g. a saved review summary).
        #[arg(long, value_name = "FILE", conflicts_with = "body")]
        summary: Option<PathBuf>,
        /// Base branch to merge into (defaults to the host's default branch).
        #[arg(long)]
        base: Option<String>,
        /// Open as a draft.
        #[arg(long)]
        draft: bool,
        /// Open the creation page in a browser instead of creating directly.
        #[arg(long)]
        web: bool,
        /// Force a provider instead of auto-detecting from the `origin` remote.
        #[arg(long, value_enum)]
        provider: Option<GitHost>,
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
    /// Start the background user service
    Start,
    /// Stop the background user service
    Stop,
    /// Restart the background user service
    Restart,
    /// Show background service status
    Status,
    /// Show background service logs
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

#[derive(Subcommand, Debug)]
pub enum QaCommand {
    /// Deterministically exercise context compaction without a real model.
    CompactSmoke {
        /// Number of synthetic user/assistant turns to seed
        #[arg(long, default_value_t = 6)]
        turns: usize,
        /// Output format
        #[arg(short, long, value_enum, default_value_t = OutputFormat::Json)]
        format: OutputFormat,
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

/// Reject an empty or whitespace-only `run` prompt at parse time, so
/// `mermaid run ""` fails with a clear usage error instead of silently
/// producing nothing. Only the emptiness check trims — the original string
/// is preserved on success, since leading/trailing whitespace can be
/// meaningful prompt content.
fn non_empty_prompt(s: &str) -> Result<String, String> {
    if s.trim().is_empty() {
        Err("prompt cannot be empty".to_string())
    } else {
        Ok(s.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn non_empty_prompt_rejects_blank() {
        assert!(non_empty_prompt("").is_err());
        assert!(non_empty_prompt("   ").is_err());
        assert!(non_empty_prompt("\t\n ").is_err());
    }

    #[test]
    fn non_empty_prompt_preserves_content_including_surrounding_space() {
        assert_eq!(non_empty_prompt("hello").unwrap(), "hello");
        assert_eq!(non_empty_prompt("  hi  ").unwrap(), "  hi  ");
    }

    #[test]
    fn cli_run_rejects_empty_prompt() {
        let err = Cli::try_parse_from(["mermaid", "run", ""])
            .expect_err("empty prompt must be rejected at parse time");
        assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation);
    }

    #[test]
    fn cli_run_accepts_normal_prompt() {
        assert!(Cli::try_parse_from(["mermaid", "run", "do a thing"]).is_ok());
    }

    #[test]
    fn resume_and_continue_flags_parse_and_conflict() {
        // Claude Code parity: `--resume` (picker) and `--continue` (last) both
        // exist and are mutually exclusive. The old `--sessions` is gone.
        let resume = Cli::try_parse_from(["mermaid", "--resume"]).expect("--resume parses");
        assert!(resume.resume && !resume.continue_session);
        let cont = Cli::try_parse_from(["mermaid", "--continue"]).expect("--continue parses");
        assert!(cont.continue_session && !cont.resume);
        assert!(
            Cli::try_parse_from(["mermaid", "--resume", "--continue"]).is_err(),
            "--resume and --continue must conflict"
        );
        assert!(
            Cli::try_parse_from(["mermaid", "--sessions"]).is_err(),
            "the old --sessions flag is renamed to --resume"
        );
    }
}
