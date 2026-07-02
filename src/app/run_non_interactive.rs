//! Headless driver for `mermaid run <prompt>`.
//!
//! Same reducer + same effect runner + same providers + same tools
//! as the interactive path. Differences: no `TerminalGuard`, no
//! crossterm events, no tick timer, no render. One synthetic
//! `Msg::SubmitPrompt` seeds the reducer; the loop spins until
//! `state.turn == Idle` and the queue is empty.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use tokio::time::timeout;

use crate::app::Config;
use crate::app::lifecycle::RuntimeLifecycle;
use crate::cli::OutputFormat;
use crate::domain::{Msg, State, TurnState, update};
use crate::effect::EffectRunner;
use crate::models::MessageRole;
use crate::providers::ToolRegistry;

/// Output shape the CLI prints.
#[derive(Debug, Default)]
pub struct RunResult {
    pub response: String,
    pub reasoning: Option<String>,
    pub total_tokens: usize,
    pub errors: Vec<String>,
}

/// Per-invocation options for `run_non_interactive`.
///
/// Added as a struct so new flags can land without reshuffling the
/// function's positional args. All fields default to "no change".
#[derive(Debug, Default, Clone)]
pub struct RunOptions {
    /// When true, register an empty `ToolRegistry` — the model sees no
    /// tools and can't take actions. Dry-run mode for
    /// `mermaid run --no-execute`.
    pub no_execute: bool,
    /// Durable runtime task that owns this run, when launched through
    /// `mermaidd` or `mermaid run` task creation.
    pub task_id: Option<String>,
}

/// Drive one prompt to completion with explicit per-call options. Bounded by a
/// generous 20-minute wall-clock so a runaway model doesn't hang a script.
pub async fn run_non_interactive_with(
    config: Config,
    cwd: PathBuf,
    model_id: String,
    prompt: String,
    opts: RunOptions,
) -> Result<RunResult> {
    let providers = std::sync::Arc::new(crate::providers::ProviderFactory::new(config.clone()));
    // F6 `--no-execute`: build an empty tool registry so the model can
    // plan but never act. MCP init below is also skipped to match.
    let tools = if opts.no_execute {
        std::sync::Arc::new(ToolRegistry::new())
    } else {
        ToolRegistry::build(
            &config,
            crate::providers::TuiMode::Headless,
            providers.clone(),
        )
    };
    let (mut runner, mut msg_rx) =
        EffectRunner::pair_from_with_task(cwd.clone(), providers, tools, opts.task_id.clone());
    runner = runner.without_terminal_title();

    let mut state = State::new(config.clone(), cwd.clone(), model_id);
    let mut lifecycle = RuntimeLifecycle::new();

    // Load project instructions + the memory index synchronously. The
    // interactive TUI gets these from the config watcher's first poll
    // (`run.rs`), which the headless driver never spawns — so without this the
    // model call would go out with no MERMAID.md/AGENTS.md and no memory, while
    // `mermaid doctor` reports them loaded. `build_chat_request` reads them off
    // `state`, so they must be in place before the seed below.
    let (instructions, memory) =
        crate::app::instructions::load_project_context(&cwd, &config.memory);
    state.instructions = instructions;
    state.memory = memory;

    // Bootstrap effects (MCP init) before the first prompt.
    //
    // Skip MCP init when `--no-execute` — MCP tools would advertise
    // through the registry we just emptied, so spinning up their
    // processes is wasted work.
    if !config.mcp_servers.is_empty() && !opts.no_execute {
        runner.dispatch(crate::domain::Cmd::InitMcpServers(
            config.mcp_servers.clone(),
        ));
    }

    // Seed the turn.
    let seed = Msg::SubmitPrompt {
        text: prompt,
        attachment_ids: vec![],
    };
    // Inject the wall clock as data so the reducer stays pure (Cause 3).
    state.now = chrono::Local::now();
    let (new_state, cmds) = update(state, seed);
    state = new_state;
    for cmd in cmds {
        runner.dispatch(cmd);
    }

    let deadline = Duration::from_secs(20 * 60);

    let drive = async {
        while !matches!(state.turn, TurnState::Idle) || !state.ui.queued_messages.is_empty() {
            let msg = tokio::select! {
                m = msg_rx.recv() => match m {
                    Some(m) => m,
                    None => break,
                },
                s = lifecycle.next_msg() => match s {
                    Some(s) => s,
                    None => continue,
                },
            };
            state.now = chrono::Local::now();
            let (new_state, cmds) = update(state, msg);
            state = new_state;
            for cmd in cmds {
                runner.dispatch(cmd);
            }
            if state.should_exit {
                break;
            }
        }
        state
    };

    let final_state = timeout(deadline, drive).await.map_err(|_| {
        anyhow::anyhow!(
            "non-interactive run exceeded {} seconds",
            deadline.as_secs()
        )
    })?;

    runner.shutdown().await;
    Ok(build_result(&final_state))
}

/// Walk the committed message history and pull out the last
/// assistant response + any errors encountered.
fn build_result(state: &State) -> RunResult {
    let mut out = RunResult {
        total_tokens: state.session.cumulative_token_usage.total_tokens,
        ..RunResult::default()
    };

    for msg in state.session.messages() {
        for action in &msg.actions {
            if let crate::domain::ActionResult::Error { error } = &action.result {
                out.errors
                    .push(format!("{}: {}", action.action_type, error));
            }
        }
    }

    if let Some(last) = state
        .session
        .messages()
        .iter()
        .rev()
        .find(|m| m.role == MessageRole::Assistant)
    {
        out.response = last.content.clone();
        out.reasoning = last.thinking.clone();
    }

    out
}

/// Render a `RunResult` in the requested output format.
pub fn format_result(result: &RunResult, format: OutputFormat) -> String {
    match format {
        OutputFormat::Text => {
            if result.response.is_empty() && !result.errors.is_empty() {
                result.errors.join("\n")
            } else {
                result.response.clone()
            }
        },
        OutputFormat::Markdown => {
            let mut out = result.response.clone();
            if !result.errors.is_empty() {
                out.push_str("\n\n---\n\n## Errors\n\n");
                for e in &result.errors {
                    out.push_str(&format!("- {}\n", e));
                }
            }
            out
        },
        OutputFormat::Json => {
            let json = serde_json::json!({
                "response": result.response,
                "reasoning": result.reasoning,
                "total_tokens": result.total_tokens,
                "errors": result.errors,
            });
            serde_json::to_string_pretty(&json).unwrap_or_default()
        },
    }
}
