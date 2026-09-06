use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

use mermaid_model::models::ReasoningLevel;

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

    /// Resume a past conversation. Bare `--resume` opens a searchable picker
    /// (interactive only); `--resume <SESSION_ID>` loads that session directly
    /// and also works headless with `mermaid run`. Like `claude --resume`.
    /// `None` = flag absent; `Some(None)` = bare (picker);
    /// `Some(Some(id))` = direct load.
    #[arg(
        long,
        value_name = "SESSION_ID",
        num_args = 0..=1,
        conflicts_with = "continue_session"
    )]
    pub resume: Option<Option<String>>,

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

    /// Override a config value: repeatable `-c key.path=value` applied on top
    /// of the config file (value parsed as TOML, so `true`/`3`/`"x"` keep their
    /// types; a bare word is a string). Example:
    /// `-c default_model.max_tokens=8192 -c safety.mode=full_access`.
    #[arg(short = 'c', long = "config", value_name = "KEY=VALUE", global = true)]
    pub config_overrides: Vec<String>,

    /// Deny web-tool egress on every platform and network access for model-run
    /// shell commands where an OS sandbox is available. Equivalent to
    /// `-c safety.network=deny`.
    #[arg(long, global = true)]
    pub no_network: bool,

    /// Confine model-run shell commands' writes to the project directory (plus
    /// the system temp directory and, on unix, `/dev`) this session. Linux
    /// Landlock (best-effort on kernels before 5.13), macOS Seatbelt, Windows
    /// AppContainer. Equivalent to `-c safety.filesystem=project`.
    #[arg(long, global = true)]
    pub confine_fs: bool,

    /// Full OS sandbox for model-run shell commands: shorthand for
    /// `--no-network --confine-fs`.
    #[arg(long, global = true)]
    pub sandbox: bool,

    /// Apply a named config overlay from `[profiles.<name>]` in your user
    /// config file for this invocation. Profile values beat the user file but
    /// lose to a repo's project config and to `-c` overrides.
    #[arg(long, value_name = "NAME", global = true)]
    pub profile: Option<String>,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

impl Cli {
    /// Collect this invocation's config-shaped flags into the `Session`
    /// config layer's inputs: the repeatable `-c` overrides plus the dedicated
    /// flags (`--no-network`/`--confine-fs`/`--sandbox`, and `run`'s
    /// `--max-tokens`/`--allow-untrusted-tools`). Prompt flags and
    /// `--reasoning` stay outside the layer merge — see `apply_prompt_flags`.
    #[must_use]
    pub fn session_flags(&self) -> mermaid_domain::SessionFlags {
        let (max_tokens, allow_untrusted_tools) = match &self.command {
            Some(Commands::Run {
                max_tokens,
                allow_untrusted_tools,
                ..
            }) => (*max_tokens, *allow_untrusted_tools),
            _ => (None, false),
        };
        mermaid_domain::SessionFlags {
            overrides: self.config_overrides.clone(),
            deny_network: self.no_network || self.sandbox,
            confine_fs: self.confine_fs || self.sandbox,
            max_tokens,
            allow_untrusted_tools,
            profile: self.profile.clone(),
        }
    }
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
        /// Model id, e.g. `<provider>/<model>`
        model: String,
    },
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
    /// Write a local diagnostic bundle: doctor report, config summary
    /// (names and booleans only), recent trace events, and the log tail.
    /// Nothing is uploaded — the file stays on this machine.
    Feedback {
        /// Print to stdout instead of writing `mermaid-feedback-<ts>` in the
        /// current directory
        #[arg(long)]
        stdout: bool,
        /// Output format (markdown or json)
        #[arg(short, long, value_enum, default_value_t = OutputFormat::Markdown)]
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
        /// Attach to the task's live event stream (daemon required): prints
        /// NDJSON `RunEvent` lines to stdout and exits after the terminal
        /// `result`. Works on queued tasks too (waits for the run to start).
        #[arg(long)]
        follow: bool,
        /// Send a prompt to a RUNNING task (daemon required): the text lands
        /// in the running agent's queue and it answers in a turn of its own,
        /// as if you had typed it. Finished tasks take `mermaid run --resume`
        /// instead.
        #[arg(long, value_name = "TEXT")]
        send: Option<String>,
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
    /// Cancel a queued or running daemon task
    Cancel {
        /// Task id from `mermaid tasks`
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
        /// MCP server name (registry key, or a label when using --command)
        name: String,
        /// Skip the confirmation prompt before fetching and running a package
        /// that is not in the built-in registry (for scripted/CI use). Without
        /// this, adding an unknown package fails closed when there is no TTY.
        #[arg(long)]
        yes: bool,
        /// Register a raw command server instead of resolving from the registry:
        /// the executable to run (e.g. `uvx`, `node`, `/path/to/mcp-server`).
        #[arg(long)]
        command: Option<String>,
        /// Argument for --command, repeatable and order-preserving
        /// (e.g. `--arg mcp-server-git --arg --repository --arg .`).
        #[arg(long = "arg")]
        arg: Vec<String>,
        /// Environment variable for --command: repeatable `KEY=VALUE`.
        #[arg(long = "env")]
        env: Vec<String>,
        /// Register a remote Streamable HTTP server instead: the MCP endpoint
        /// URL (`https://...`, or `http://` to localhost only).
        #[arg(long, conflicts_with_all = ["command", "arg", "env"])]
        url: Option<String>,
        /// Literal HTTP header for --url: repeatable `'Name: Value'`
        /// (e.g. `--header 'Authorization: Bearer TOKEN'`).
        #[arg(long = "header", requires = "url")]
        header: Vec<String>,
        /// HTTP header for --url whose value is read from an environment
        /// variable at request time: repeatable `Header=ENV_VAR`, so the
        /// secret never lands in config.toml.
        #[arg(long = "env-header", requires = "url")]
        env_header: Vec<String>,
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
    /// Store a provider API key in the OS keyring, or list key status
    Login {
        /// Provider name (e.g. groq, anthropic, ollama). Omit to list every
        /// provider's key status.
        provider: Option<String>,
    },
    /// Remove a provider API key from the OS keyring
    Logout {
        /// Provider name whose stored key to remove
        provider: String,
    },
    /// Run a single prompt non-interactively
    Run {
        /// Prompt to execute. Omit or pass `-` to read it from piped stdin;
        /// piped stdin alongside a prompt is appended as a fenced block.
        prompt: Option<String>,

        /// Output format (text, json, markdown, ndjson)
        #[arg(short, long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,

        /// Maximum tokens to generate
        #[arg(long)]
        max_tokens: Option<usize>,

        /// Don't execute agent actions (dry run)
        #[arg(long)]
        no_execute: bool,

        /// Allow non-replayable tools (web/mcp/subagent) to run on
        /// an `Ask` decision in this headless run. Off by default — `ask` mode
        /// otherwise refuses them when there's no approval UI.
        #[arg(long)]
        allow_untrusted_tools: bool,

        /// JSON Schema file the final answer must conform to. The agentic
        /// loop runs normally; one extra formatting turn (no tools, native
        /// constrained output where the provider supports it) reshapes the
        /// final answer, validated client-side. Failures are reported in the
        /// run's errors; the text answer is still returned.
        #[arg(long, value_name = "FILE")]
        output_schema: Option<PathBuf>,

        /// Run in plan mode: the agent explores read-only and produces a plan
        /// file (`.mermaid/plans/`) instead of making changes. With no
        /// approval UI the plan is accepted as the run's deliverable but
        /// implementation does NOT start.
        #[arg(long)]
        plan: bool,

        /// With --plan: the moment the plan is presented, accept it and
        /// continue straight into implementation in the same run.
        #[arg(long, requires = "plan")]
        plan_autoaccept: bool,
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
    /// JSON structured output (a single object)
    Json,
    /// Markdown formatted output
    Markdown,
    /// Streaming newline-delimited JSON (one `RunEvent` per line) — the
    /// scripting / SDK surface for `mermaid run`.
    Ndjson,
}

/// Reject an empty or whitespace-only `run` prompt at parse time, so
/// Resolve the effective `run` prompt from the CLI arg and any piped stdin.
/// `stdin` is `Some(text)` when stdin was piped (non-TTY), else `None`. Pure so
/// it can be unit-tested; the caller does the terminal check + stdin read.
///
/// - No prompt (or `-`): use stdin; error if nothing was piped.
/// - A prompt plus piped stdin: append the stdin as a fenced block.
/// - A prompt alone: use it. Empty results are rejected with a usage error.
///
/// # Errors
///
/// The usage message to print: no prompt (or `-`) with nothing piped, and an
/// explicitly empty or whitespace-only prompt. Whitespace-only piped stdin
/// counts as nothing piped.
pub fn resolve_run_prompt(prompt: Option<&str>, stdin: Option<String>) -> Result<String, String> {
    let piped = stdin
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    match prompt.map(str::trim) {
        None | Some("-") => {
            piped.ok_or_else(|| "no prompt given: pass a prompt or pipe text on stdin".to_string())
        },
        Some("") => Err("prompt must not be empty".to_string()),
        Some(text) => Ok(match piped {
            Some(extra) => format!("{text}\n\n```\n{extra}\n```"),
            None => text.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn resolve_run_prompt_reads_stdin_when_dash_or_missing() {
        assert_eq!(
            resolve_run_prompt(None, Some("piped work".to_string())).unwrap(),
            "piped work"
        );
        assert_eq!(
            resolve_run_prompt(Some("-"), Some("  piped  ".to_string())).unwrap(),
            "piped"
        );
    }

    #[test]
    fn resolve_run_prompt_errors_without_prompt_or_stdin() {
        assert!(resolve_run_prompt(None, None).is_err());
        assert!(resolve_run_prompt(Some("-"), None).is_err());
        assert!(resolve_run_prompt(Some(""), None).is_err());
        assert!(resolve_run_prompt(None, Some("   ".to_string())).is_err());
    }

    #[test]
    fn resolve_run_prompt_appends_piped_stdin_to_explicit_prompt() {
        let out = resolve_run_prompt(Some("summarize"), Some("file body".to_string())).unwrap();
        assert!(out.starts_with("summarize"));
        assert!(out.contains("file body"));
    }

    #[test]
    fn cli_run_allows_missing_prompt_and_normal_prompt() {
        // The prompt is optional now (stdin fallback); parsing succeeds with no
        // positional, and emptiness is enforced later by `resolve_run_prompt`.
        assert!(Cli::try_parse_from(["mermaid", "run"]).is_ok());
        assert!(Cli::try_parse_from(["mermaid", "run", "do a thing"]).is_ok());
    }

    #[test]
    fn cli_config_overrides_are_repeatable() {
        let cli = Cli::try_parse_from(["mermaid", "-c", "a.b=1", "-c", "c=true", "run", "x"])
            .expect("repeatable -c parses");
        assert_eq!(cli.config_overrides, vec!["a.b=1", "c=true"]);
    }

    #[test]
    fn cli_config_override_after_subcommand_is_global() {
        let cli = Cli::try_parse_from(["mermaid", "run", "x", "-c", "c=true"])
            .expect("global -c parses after the subcommand");
        assert_eq!(cli.config_overrides, vec!["c=true"]);
    }

    #[test]
    fn parses_login_and_logout() {
        let cli = Cli::parse_from(["mermaid", "login"]);
        assert!(matches!(
            cli.command,
            Some(Commands::Login { provider: None })
        ));
        let cli = Cli::parse_from(["mermaid", "login", "groq"]);
        assert!(matches!(cli.command, Some(Commands::Login { provider: Some(p) }) if p == "groq"));
        let cli = Cli::parse_from(["mermaid", "logout", "groq"]);
        assert!(matches!(cli.command, Some(Commands::Logout { provider }) if provider == "groq"));
    }

    #[test]
    fn parses_task_follow() {
        let cli = Cli::parse_from(["mermaid", "task", "t1", "--follow"]);
        assert!(matches!(cli.command, Some(Commands::Task { id, follow: true, .. }) if id == "t1"));
        let cli = Cli::parse_from(["mermaid", "task", "t1"]);
        assert!(
            matches!(cli.command, Some(Commands::Task { id, follow: false, .. }) if id == "t1")
        );
    }

    #[test]
    fn session_flags_collect_sandbox_and_run_flags() {
        let cli = Cli::try_parse_from([
            "mermaid",
            "--sandbox",
            "-c",
            "a=1",
            "run",
            "x",
            "--max-tokens",
            "512",
            "--allow-untrusted-tools",
        ])
        .expect("parses");
        let flags = cli.session_flags();
        assert!(flags.deny_network && flags.confine_fs);
        assert_eq!(flags.max_tokens, Some(512));
        assert!(flags.allow_untrusted_tools);
        assert_eq!(flags.overrides, vec!["a=1"]);

        // Without `run`, the run-scoped flags stay unset.
        let cli = Cli::try_parse_from(["mermaid", "--no-network"]).expect("parses");
        let flags = cli.session_flags();
        assert!(flags.deny_network && !flags.confine_fs);
        assert_eq!(flags.max_tokens, None);
        assert!(!flags.allow_untrusted_tools);
    }

    #[test]
    fn add_url_conflicts_with_command_and_requires_url_for_headers() {
        // Remote registration parses with its header flags...
        let cli = Cli::try_parse_from([
            "mermaid",
            "add",
            "gh",
            "--url",
            "https://example.com/mcp",
            "--header",
            "X-Token: abc",
            "--env-header",
            "Authorization=TOKEN_VAR",
        ])
        .expect("parses");
        match cli.command {
            Some(Commands::Add {
                url,
                header,
                env_header,
                ..
            }) => {
                assert_eq!(url.as_deref(), Some("https://example.com/mcp"));
                assert_eq!(header, vec!["X-Token: abc".to_string()]);
                assert_eq!(env_header, vec!["Authorization=TOKEN_VAR".to_string()]);
            },
            other => panic!("expected Add, got {other:?}"),
        }
        // ...but --url and --command are mutually exclusive registration paths,
        assert!(
            Cli::try_parse_from([
                "mermaid",
                "add",
                "gh",
                "--url",
                "https://example.com/mcp",
                "--command",
                "npx"
            ])
            .is_err(),
            "--url must conflict with --command"
        );
        // and the header flags only make sense with --url.
        assert!(
            Cli::try_parse_from(["mermaid", "add", "gh", "--header", "X: y"]).is_err(),
            "--header must require --url"
        );
    }

    #[test]
    fn resume_and_continue_flags_parse_and_conflict() {
        // Claude Code parity: `--resume` (picker), `--resume <id>` (direct),
        // and `--continue` (last) all exist; resume/continue are mutually
        // exclusive. The old `--sessions` is gone.
        let resume = Cli::try_parse_from(["mermaid", "--resume"]).expect("--resume parses");
        assert_eq!(resume.resume, Some(None));
        assert!(!resume.continue_session);
        let direct = Cli::try_parse_from(["mermaid", "--resume", "20260709_120000_000"])
            .expect("--resume <id> parses");
        assert_eq!(direct.resume, Some(Some("20260709_120000_000".to_string())));
        let absent = Cli::try_parse_from(["mermaid"]).expect("no flag parses");
        assert_eq!(absent.resume, None);
        let cont = Cli::try_parse_from(["mermaid", "--continue"]).expect("--continue parses");
        assert!(cont.continue_session && cont.resume.is_none());
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
