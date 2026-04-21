//! The ~30-line main loop.
//!
//! This is the single entry point the v0.7 architecture composes:
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
//!
//! Behind `MERMAID_V7=1` (env var) or `--experimental-v7` (CLI flag,
//! wired in C10). Users can flip between this and the old runtime
//! for side-by-side testing during the migration window.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use crossterm::event::EventStream;
use futures::StreamExt;
use tokio::time::{Duration, interval};

use crate::app::Config;
use crate::app::event_source::event_to_msg;
use crate::app::terminal::TerminalGuard;
use crate::domain::{Cmd, Msg, State, update};
use crate::effect::EffectRunner;
use crate::providers::ToolRegistry;
use crate::render::{RenderState, render};

/// The v0.7 main loop. External behavior is intended to match the
/// v0.6 runtime; differences are bugs.
pub async fn run(config: Config, cwd: PathBuf, model_id: String) -> Result<()> {
    let mut state = State::new(config.clone(), cwd.clone(), model_id);
    let tools = Arc::new(ToolRegistry::default());
    let (mut runner, mut msg_rx) =
        EffectRunner::pair_with_bindings(cwd.clone(), config.clone(), tools);
    let mut terminal = TerminalGuard::setup()?;
    let mut rstate = RenderState::new();
    let mut events = EventStream::new();
    let mut tick = interval(Duration::from_millis(16));

    // Boot effects — in C10 this will include initial MCP server
    // startup + session load, etc. For now, just nudge the
    // instructions loader.
    for cmd in bootstrap_cmds(&config) {
        runner.dispatch(cmd);
    }

    // Main loop.
    loop {
        // Render the current state. ratatui's draw closure captures
        // &state, so we don't thread &mut state through the renderer.
        terminal
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
                Some(Ok(evt)) => event_to_msg(evt),
                _ => None,
            },
            // 3. Tick — drives elapsed-time displays + self-dismissing
            //    status lines without busy-waiting.
            _ = tick.tick() => Some(Msg::Tick),
        };

        let Some(msg) = msg else { continue };

        let (new_state, cmds) = update(state, msg);
        state = new_state;
        for cmd in cmds {
            runner.dispatch(cmd);
        }

        if state.should_exit {
            break;
        }
    }

    // Orderly shutdown — wait for any pending saves / scope cleanup.
    runner.shutdown().await;
    drop(terminal);
    Ok(())
}

/// Commands dispatched on startup before the first iteration of the
/// loop. Grows in later commits as MCP init + session load are
/// folded in.
fn bootstrap_cmds(_config: &Config) -> Vec<Cmd> {
    vec![Cmd::RefreshInstructions]
}

/// True when the user has opted into the v0.7 path for this
/// session. Checks CLI flag (wired in C10) and the `MERMAID_V7`
/// environment variable. Returns false by default so the old
/// runtime stays in charge until we flip the switch.
pub fn opted_in() -> bool {
    std::env::var("MERMAID_V7").is_ok_and(|v| v == "1")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opted_in_respects_env_var() {
        // This test is racy if the project sets MERMAID_V7 globally;
        // guard with a temp-env lock.
        temp_env::with_var("MERMAID_V7", Some("1"), || {
            assert!(opted_in());
        });
        temp_env::with_var("MERMAID_V7", Some("0"), || {
            assert!(!opted_in());
        });
        temp_env::with_var::<_, &str, _, _>("MERMAID_V7", None, || {
            assert!(!opted_in());
        });
    }

    #[test]
    fn bootstrap_includes_refresh_instructions() {
        let cmds = bootstrap_cmds(&Config::default());
        assert!(cmds.iter().any(|c| matches!(c, Cmd::RefreshInstructions)));
    }
}
