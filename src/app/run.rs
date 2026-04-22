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
use crate::app::recorder::Recorder;
use crate::app::terminal::TerminalGuard;
use crate::domain::{Cmd, Msg, State, update};
use crate::effect::EffectRunner;
use crate::providers::ToolRegistry;
use crate::render::{RenderCache, render};

/// Interactive TUI main loop. `recorder` (if provided) appends one
/// JSONL line per reducer input to the file for debugging / replay.
pub async fn run_interactive(
    config: Config,
    cwd: PathBuf,
    model_id: String,
    mut recorder: Option<Recorder>,
) -> Result<()> {
    let mut state = State::new(config.clone(), cwd.clone(), model_id);
    let providers = std::sync::Arc::new(crate::providers::ProviderFactory::new(config.clone()));
    let tools = ToolRegistry::build(
        &config,
        crate::providers::TuiMode::Interactive,
        providers.clone(),
    );
    let (mut runner, mut msg_rx) = EffectRunner::pair_from(cwd.clone(), providers, tools);
    let mut terminal = TerminalGuard::setup()?;
    let mut rstate = RenderCache::new();
    let mut events = EventStream::new();
    let mut tick = interval(Duration::from_millis(16));

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

        // Optional recording: one JSONL line per Msg, before the
        // reducer runs so the log captures even no-op inputs.
        if let Some(r) = recorder.as_mut() {
            let body = serde_json::json!({
                "kind": format!("{:?}", msg.kind()),
            });
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

    // Orderly shutdown — wait for any pending saves / scope cleanup.
    runner.shutdown().await;
    drop(terminal);
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
