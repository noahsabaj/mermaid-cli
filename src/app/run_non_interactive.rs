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
use crate::domain::{Msg, RUN_EVENT_PROTOCOL_VERSION, RunEvent, State, TurnState, update};
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
    /// Conversation/session id that owns this run — resumable with
    /// `mermaid run --resume <id>`.
    pub session_id: String,
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
    /// External cancellation. When it fires, the driver injects
    /// `Msg::CancelTurn` — the same message the TUI's Esc sends — so the
    /// reducer unwinds the turn gracefully (tool process tree killed, turn
    /// `JoinSet` drained). If the reducer hasn't reached `Idle` within a grace
    /// window after that, the drive loop hard-stops.
    pub cancel: Option<tokio_util::sync::CancellationToken>,
    /// Wall-clock budget override. `None` keeps the built-in 20-minute
    /// deadline.
    pub deadline: Option<Duration>,
    /// When true, the driver streams the run lifecycle to stdout as
    /// newline-delimited `RunEvent` JSON (`mermaid run --format ndjson`). Off
    /// for the daemon scheduler and every other caller, which own their own
    /// output.
    pub stream_ndjson: bool,
    /// Saved conversation to seed the session with (`--resume <id>` /
    /// `--continue`). The run appends to the SAME session id, so repeated
    /// `--resume <id>` invocations chain naturally.
    pub seed: Option<crate::session::ConversationHistory>,
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

    // Captured before `model_id` is moved into `State`, for the NDJSON stream.
    let stream_ndjson = opts.stream_ndjson;
    let event_model = model_id.clone();

    let mut state = State::new(config.clone(), cwd.clone(), model_id, chrono::Local::now());
    // `--resume <id>` / `--continue`: seed the session from the saved
    // conversation (same machinery as the interactive path — meters restored,
    // orphan tool pairs repaired via normalize_history), then backfill
    // provenance blanks. The seeded id survives, so the run appends to the
    // same `.mermaid/conversations/<id>.json`.
    if let Some(history) = opts.seed.clone() {
        state.seed_conversation(history);
    }
    crate::app::stamp_session_provenance(&mut state, &cwd);
    let session_id = state.session.conversation.id.clone();
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

    // First line of the NDJSON stream: protocol + run identity.
    if stream_ndjson {
        emit_run_event(&RunEvent::SessionStarted {
            protocol_version: RUN_EVENT_PROTOCOL_VERSION,
            cli_version: env!("CARGO_PKG_VERSION").to_string(),
            model: event_model,
            task_id: opts.task_id.clone(),
            session_id: session_id.clone(),
        });
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

    let deadline = opts.deadline.unwrap_or(Duration::from_secs(20 * 60));

    /// How long a cancelled run may keep unwinding before the drive loop
    /// hard-stops. Generous next to the turn scope's own ~2s teardown bound.
    const CANCEL_GRACE: Duration = Duration::from_secs(15);
    let cancel = opts.cancel.clone();
    // Set when the cancel token fires; from then on the loop exits as soon as
    // the turn is idle (queued messages must not seed another turn) or the
    // grace deadline passes.
    let mut cancel_deadline: Option<tokio::time::Instant> = None;

    let drive = async {
        loop {
            let idle = matches!(state.turn, TurnState::Idle);
            if drive_should_stop(
                idle,
                state.ui.queued_messages.is_empty(),
                cancel_deadline.is_some(),
            ) {
                break;
            }
            let msg = tokio::select! {
                m = msg_rx.recv() => match m {
                    Some(m) => m,
                    None => break,
                },
                s = lifecycle.next_msg() => match s {
                    Some(s) => s,
                    None => continue,
                },
                _ = async {
                    match &cancel {
                        Some(token) => token.cancelled().await,
                        None => std::future::pending().await,
                    }
                }, if cancel.is_some() && cancel_deadline.is_none() => {
                    cancel_deadline = Some(tokio::time::Instant::now() + CANCEL_GRACE);
                    Msg::CancelTurn
                },
                // NOTE: select! evaluates every branch expression even when its
                // `if` guard is false, so the sleep target must not unwrap.
                _ = tokio::time::sleep_until(
                    cancel_deadline
                        .unwrap_or_else(|| tokio::time::Instant::now() + Duration::from_secs(86_400)),
                ), if cancel_deadline.is_some() => {
                    tracing::warn!("cancelled run did not unwind within grace; hard-stopping");
                    break;
                },
            };
            // Plumbing notices ("Starting the local Ollama server…") have no
            // renderer here — mirror them to stderr live so the user isn't
            // staring at silence during an up-to-15s server start. stderr,
            // not stdout: the response payload must stay clean for scripts.
            if let Msg::TransientStatus { text } = &msg {
                eprintln!("{text}");
            }
            // Project the lifecycle message onto the public NDJSON stream before
            // `update` consumes it. Most messages have no projection.
            if stream_ndjson && let Some(event) = RunEvent::from_msg(&msg) {
                emit_run_event(&event);
            }
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
    let result = build_result(&final_state);
    // Terminal line of the NDJSON stream: the aggregated result.
    if stream_ndjson {
        emit_run_event(&RunEvent::Result {
            response: result.response.clone(),
            reasoning: result.reasoning.clone(),
            total_tokens: result.total_tokens as u64,
            errors: result.errors.clone(),
            session_id: result.session_id.clone(),
        });
    }
    Ok(result)
}

/// Write one `RunEvent` as a JSON line to stdout (the NDJSON SDK stream).
fn emit_run_event(event: &RunEvent) {
    println!("{}", serde_json::to_string(event).unwrap_or_default());
}

/// Walk the committed message history and pull out the last
/// assistant response + any errors encountered.
fn build_result(state: &State) -> RunResult {
    let mut out = RunResult {
        total_tokens: state.session.cumulative_token_usage.total_tokens,
        session_id: state.session.conversation.id.clone(),
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
            // Typed single-object form — the same shape as the streamed terminal
            // `RunEvent::Result`, so the golden test pins this output too.
            let event = RunEvent::Result {
                response: result.response.clone(),
                reasoning: result.reasoning.clone(),
                total_tokens: result.total_tokens as u64,
                errors: result.errors.clone(),
                session_id: result.session_id.clone(),
            };
            serde_json::to_string_pretty(&event).unwrap_or_default()
        },
        OutputFormat::Ndjson => {
            // Events were streamed live during the run; nothing to print here.
            String::new()
        },
    }
}

/// Whether the drive loop should stop this iteration.
///
/// A completed run stops once the turn is `idle` and nothing is queued. A
/// *cancelled* run (its grace deadline armed, so `cancelling` is true) stops as
/// soon as the turn is idle even if messages are queued — a cancel must never
/// let the queue seed a fresh turn.
fn drive_should_stop(idle: bool, queue_empty: bool, cancelling: bool) -> bool {
    idle && (queue_empty || cancelling)
}

#[cfg(test)]
mod tests {
    use super::drive_should_stop;

    #[test]
    fn drive_keeps_running_until_idle() {
        // Never stop mid-turn, whatever the queue/cancel state.
        assert!(!drive_should_stop(false, true, false));
        assert!(!drive_should_stop(false, true, true));
        assert!(!drive_should_stop(false, false, true));
    }

    #[test]
    fn drive_stops_when_idle_and_drained() {
        // Normal completion: idle with an empty queue.
        assert!(drive_should_stop(true, true, false));
    }

    #[test]
    fn drive_keeps_draining_queue_when_not_cancelling() {
        // Idle but messages queued and not cancelling → keep going so the
        // queued input seeds the next turn.
        assert!(!drive_should_stop(true, false, false));
    }

    #[test]
    fn cancel_stops_at_idle_even_with_queued_messages() {
        // The load-bearing case: once cancelling, an idle turn stops the loop
        // even with messages queued — the cancel must not start a new turn.
        assert!(drive_should_stop(true, false, true));
    }
}
