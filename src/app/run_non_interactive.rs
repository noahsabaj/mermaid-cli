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

/// Drive one prompt to completion. Bounded by a generous 20-minute
/// wall-clock so a runaway model doesn't hang a script.
pub async fn run_non_interactive(
    config: Config,
    cwd: PathBuf,
    model_id: String,
    prompt: String,
) -> Result<RunResult> {
    let tools = ToolRegistry::build(&config, crate::providers::TuiMode::Headless);
    let (mut runner, mut msg_rx) =
        EffectRunner::pair_with_bindings(cwd.clone(), config.clone(), tools);

    let mut state = State::new(config.clone(), cwd, model_id);

    // Bootstrap effects (MCP init, instructions refresh) before the
    // first prompt. Same sequence the interactive path uses.
    if !config.mcp_servers.is_empty() {
        runner.dispatch(crate::domain::Cmd::InitMcpServers(
            config.mcp_servers.clone(),
        ));
    }
    runner.dispatch(crate::domain::Cmd::RefreshInstructions);

    // Seed the turn.
    let seed = Msg::SubmitPrompt {
        text: prompt,
        attachment_ids: vec![],
    };
    let (new_state, cmds) = update(state, seed);
    state = new_state;
    for cmd in cmds {
        runner.dispatch(cmd);
    }

    let deadline = Duration::from_secs(20 * 60);

    let drive = async {
        while !matches!(state.turn, TurnState::Idle) || !state.ui.queued_messages.is_empty() {
            let msg = match msg_rx.recv().await {
                Some(m) => m,
                None => break,
            };
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
        total_tokens: state.session.cumulative_tokens,
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
