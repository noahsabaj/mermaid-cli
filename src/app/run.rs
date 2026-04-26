//! The ~30-line main loop.
//!
//! Single entry point that composes crossterm events, the reducer,
//! and the effect runner:
//!
//! ```text
//!   crossterm events ──┐
//!                      ├── tokio::select! ── Msg ── update(State, Msg) ── (State, Vec<Cmd>) ── EffectRunner::dispatch ──┐
//!   effect results  ──┤                                                                                                   │
//!                      │                                                                          ▲                         │
//!   tick              ──┘                                                                          │                         │
//!                                                                                                  └─────── Msg back ◄──────┘
//! ```
//!
//! No parallel event loops, no observer callbacks, no polling. One
//! select!, one reducer call per message, effects dispatched into
//! structured concurrency per turn.

use std::path::PathBuf;

use anyhow::Result;
use crossterm::event::EventStream;
use futures::StreamExt;
use tokio::time::{Duration, interval};

use crate::app::Config;
use crate::app::event_source::event_to_msg;
use crate::app::lifecycle::RuntimeLifecycle;
use crate::app::recorder::{Recorder, record_msg_body};
use crate::app::terminal::TerminalGuard;
use crate::domain::{Cmd, Msg, RuntimeSignal, State, update};
use crate::effect::EffectRunner;
use crate::providers::ToolRegistry;
use crate::render::{RenderCache, render};
use crate::session::ConversationHistory;

/// Options for `run_interactive`. Added so new flags land without
/// reshuffling positional args.
///
/// Not `Debug` because `Recorder` owns a `BufWriter<File>` which isn't
/// Debug. The bigger picture is that nothing prints these — they're an
/// argument bundle, not telemetry.
#[derive(Default)]
pub struct InteractiveOptions {
    /// Optional recorder for `--record <file>` JSONL replay.
    pub recorder: Option<Recorder>,
    /// Optional conversation to seed the session with (e.g. from
    /// `--continue` or `--sessions`). When `Some`, the seeded history
    /// replaces `State::session.conversation` before the first frame.
    pub seed_conversation: Option<ConversationHistory>,
}

/// Interactive TUI main loop. Backwards-compatible wrapper that
/// forwards to `run_interactive_with` with default options.
pub async fn run_interactive(
    config: Config,
    cwd: PathBuf,
    model_id: String,
    recorder: Option<Recorder>,
) -> Result<()> {
    run_interactive_with(
        config,
        cwd,
        model_id,
        InteractiveOptions {
            recorder,
            seed_conversation: None,
        },
    )
    .await
}

/// Interactive TUI main loop with explicit options. `recorder` (if
/// provided) appends one JSONL line per reducer input to the file for
/// debugging / replay.
pub async fn run_interactive_with(
    config: Config,
    cwd: PathBuf,
    model_id: String,
    mut opts: InteractiveOptions,
) -> Result<()> {
    let mut state = State::new(config.clone(), cwd.clone(), model_id);
    if let Some(history) = opts.seed_conversation.take() {
        // `--continue` / `--sessions` seed: replace the fresh
        // conversation with the loaded history. Title already reflects
        // the saved session, so re-dispatch the terminal title once.
        let title = history.title.clone();
        state.session.conversation = history;
        state.ui.last_title_dispatched = Some(title);
    }
    let providers = std::sync::Arc::new(crate::providers::ProviderFactory::new(config.clone()));
    let tools = ToolRegistry::build(
        &config,
        crate::providers::TuiMode::Interactive,
        providers.clone(),
    );
    let (mut runner, mut msg_rx) = EffectRunner::pair_from(cwd.clone(), providers, tools);
    let mut terminal = Some(TerminalGuard::setup()?);
    let mut rstate = RenderCache::new();
    let mut events = EventStream::new();
    let mut lifecycle = RuntimeLifecycle::new();
    let mut tick = interval(Duration::from_millis(16));
    let mut recorder = opts.recorder;

    // Boot effects: MCP server init (if configured) + an initial
    // instructions refresh so MERMAID.md content is in State before
    // the first prompt.
    for cmd in bootstrap_cmds(&config) {
        runner.dispatch(cmd);
    }

    // Main loop.
    loop {
        // Render the current state. ratatui's draw closure captures
        // &state, so we don't thread &mut state through the renderer.
        terminal
            .as_mut()
            .expect("terminal guard is alive while the render loop runs")
            .inner_mut()
            .draw(|f| render(&state, &mut rstate, f))?;

        let msg = tokio::select! {
            biased;
            // 1. Effect results first. Streaming chunks are hot; we
            //    want render latency low when the model is producing
            //    tokens.
            m = msg_rx.recv() => m,
            // 2. Crossterm events.
            e = events.next() => match e {
                Some(Ok(evt)) => {
                    // F13: Ctrl+Click on a chat image tile opens the
                    // image via the system viewer. Mapping from screen
                    // coords to (message_index, image_index) lives in
                    // ChatState (the render layer) — the event source
                    // can't do it alone. Synthesize `OpenImageAt` here
                    // when the click hits a tracked image.
                    if let crossterm::event::Event::Mouse(m) = &evt
                        && matches!(m.kind, crossterm::event::MouseEventKind::Down(_))
                        && m.modifiers.contains(crossterm::event::KeyModifiers::CONTROL)
                        && let Some(target) = rstate.chat.find_image_at_screen_pos(m.row)
                    {
                        Some(Msg::OpenImageAt {
                            message_index: target.message_index,
                            image_index: target.image_index,
                        })
                    } else {
                        event_to_msg(evt)
                    }
                },
                Some(Err(error)) => {
                    tracing::warn!(error = %error, "terminal event stream failed");
                    None
                },
                None => Some(Msg::RuntimeSignal(RuntimeSignal::Hangup)),
            },
            // 3. OS lifecycle signals. A typed Ctrl+C in raw mode is
            //    handled by the crossterm branch above; this covers
            //    SIGINT/SIGTERM/SIGHUP delivered externally.
            s = lifecycle.next_msg() => s,
            // 4. Tick — drives elapsed-time displays + self-dismissing
            //    status lines without busy-waiting.
            _ = tick.tick() => Some(Msg::Tick),
        };

        let Some(msg) = msg else { continue };

        // Optional recording: one JSONL line per Msg, before the
        // reducer runs so the log captures even no-op inputs.
        if let Some(r) = recorder.as_mut() {
            let body = record_msg_body(&msg);
            let _ = r.record_kind(msg.kind(), msg.turn_id(), body);
        }

        let (new_state, cmds) = update(state, msg);
        state = new_state;
        for cmd in cmds {
            runner.dispatch(cmd);
        }

        if state.should_exit {
            break;
        }
    }

    // Restore the user's terminal before async shutdown. Shutdown can
    // wait on pending saves / cancelled scopes for a bounded period;
    // keeping raw mode + mouse capture alive during that wait makes
    // Ctrl+C feel ignored and can leak mouse escape sequences into
    // the shell if the user keeps interacting.
    drop(events);
    if let Some(mut terminal) = terminal.take() {
        terminal.restore_now();
    }

    // Orderly shutdown — wait for any pending saves / scope cleanup.
    runner.shutdown().await;
    Ok(())
}

/// Commands dispatched on startup before the first iteration of the
/// loop. Fires MCP init (if configured) + an initial instructions
/// sweep so MERMAID.md content lands before the first prompt.
fn bootstrap_cmds(config: &Config) -> Vec<Cmd> {
    let mut cmds = vec![Cmd::RefreshInstructions];
    if !config.mcp_servers.is_empty() {
        cmds.push(Cmd::InitMcpServers(config.mcp_servers.clone()));
    }
    cmds
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_includes_refresh_instructions() {
        let cmds = bootstrap_cmds(&Config::default());
        assert!(cmds.iter().any(|c| matches!(c, Cmd::RefreshInstructions)));
    }

    #[test]
    fn bootstrap_skips_mcp_init_when_no_servers_configured() {
        let cmds = bootstrap_cmds(&Config::default());
        assert!(!cmds.iter().any(|c| matches!(c, Cmd::InitMcpServers(_))));
    }

    #[test]
    fn bootstrap_includes_mcp_init_when_servers_configured() {
        let mut cfg = Config::default();
        cfg.mcp_servers.insert(
            "example".to_string(),
            crate::app::McpServerConfig {
                command: "echo".to_string(),
                args: vec![],
                env: std::collections::HashMap::new(),
            },
        );
        let cmds = bootstrap_cmds(&cfg);
        assert!(cmds.iter().any(|c| matches!(c, Cmd::InitMcpServers(_))));
    }
}
