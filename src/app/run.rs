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

use std::collections::VecDeque;
use std::path::PathBuf;

use anyhow::Result;
use crossterm::event::EventStream;
use futures::{FutureExt, StreamExt};
use tokio::time::{Duration, interval};

use crate::app::Config;
use crate::app::event_source::coalesce_key_burst;
use crate::app::lifecycle::RuntimeLifecycle;
use crate::app::recorder::{RECORDING_FORMAT_VERSION, Recorder, SessionHeader};
use crate::app::terminal::TerminalGuard;
use crate::domain::{Cmd, Msg, RuntimeSignal, State, update};
use crate::effect::EffectRunner;
use crate::providers::ToolRegistry;
use crate::render::{RenderCache, render};
use crate::session::ConversationHistory;

/// Options for `run_interactive_with`. Added so new flags land without
/// reshuffling positional args.
///
/// Not `Debug` because `Recorder` owns a `BufWriter<File>` which isn't
/// Debug. The bigger picture is that nothing prints these — they're an
/// argument bundle, not telemetry.
#[derive(Default)]
pub struct InteractiveOptions {
    /// Optional recorder for `--record <file>` JSONL capture.
    pub recorder: Option<Recorder>,
    /// Optional conversation to seed the session with (e.g. from
    /// `--continue` or `--sessions`). When `Some`, the seeded history
    /// replaces `State::session.conversation` before the first frame.
    pub seed_conversation: Option<ConversationHistory>,
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
    // One startup clock read, shared by `State::new` and the recording
    // header: replay seeds `State::new` with the recorded value and gets the
    // same initial conversation id/title.
    let startup_now = chrono::Local::now();
    let mut state = State::new(config.clone(), cwd.clone(), model_id.clone(), startup_now);
    let seed = opts.seed_conversation.take();
    if let Some(r) = opts.recorder.as_mut() {
        // The header makes a recording self-contained: `--replay` rebuilds
        // the initial State from it (config, model, cwd, seed) without
        // reading this machine's live config. Written before the first Msg
        // so even a crashed session leaves a parseable log.
        r.record_header(&SessionHeader {
            format: RECORDING_FORMAT_VERSION,
            ts: startup_now,
            model_id: model_id.clone(),
            cwd: cwd.clone(),
            config: config.clone(),
            seed_conversation: seed.clone(),
        })?;
    }
    if let Some(history) = seed {
        // `--continue` / `--resume` seed — shared with `--replay` via
        // `State::seed_conversation` so both build the same starting state.
        state.seed_conversation(history);
    }
    // Stamp the git branch for the `--resume` picker. Done here (impure
    // startup) rather than in `ConversationHistory::new` so the reducer stays
    // deterministic for `--replay`. Only fills a blank — a resumed session
    // keeps the branch it was saved with; an older session with no stored
    // branch gets backfilled on its next save.
    if state.session.conversation.git_branch.is_none() {
        state.session.conversation.git_branch = crate::session::detect_git_branch(&cwd);
    }
    // Same impure-startup backfill for the rest of the session provenance: the
    // git SHA at creation and the CLI version. Only fills blanks so a resumed
    // session keeps what it was saved with.
    if state.session.conversation.git_sha.is_none() {
        state.session.conversation.git_sha = crate::session::detect_git_sha(&cwd);
    }
    if state.session.conversation.cli_version.is_none() {
        state.session.conversation.cli_version = Some(env!("CARGO_PKG_VERSION").to_string());
    }
    let providers = std::sync::Arc::new(crate::providers::ProviderFactory::new(config.clone()));
    let tools = ToolRegistry::build(
        &config,
        crate::providers::TuiMode::Interactive,
        providers.clone(),
    );
    let (runner, mut msg_rx) = EffectRunner::pair_from(cwd.clone(), providers, tools);
    // Interactive TUI: enable inline approval prompts so `ask` mode (and Auto
    // escalations) pause and prompt instead of erroring out, and inline
    // `ask_user_question` prompts so the model can ask the user structured
    // questions mid-run instead of proceeding without them.
    let mut runner = runner
        .with_interactive_approvals()
        .with_interactive_questions();
    // Keep instructions/memory fresh via the background config watcher (#45):
    // it emits Msg::InstructionsChanged/MemoryChanged on change, so the reducer
    // reads them as injected data and never does the refresh I/O inline.
    runner.spawn_config_watcher(cwd.clone(), config.memory.clone());
    let mut terminal = Some(TerminalGuard::setup()?);
    let mut rstate = RenderCache::new();
    let mut events = EventStream::new();
    let mut lifecycle = RuntimeLifecycle::new();
    let mut tick = interval(Duration::from_millis(16));
    let mut recorder = opts.recorder;

    // Boot effects: MCP server init (if configured). Instructions/memory are
    // loaded by the config watcher started above (#45), not here.
    for cmd in bootstrap_cmds(&config) {
        runner.dispatch(cmd);
    }

    // Which `select!` arm fired. Terminal events are handled *after* the
    // select! returns so the paste-coalescing drain can borrow `events`
    // again without tripping the borrow checker.
    //
    // `Msg` is the large variant, but this enum lives on the stack for one
    // loop iteration and `Msg` is passed by value everywhere already —
    // boxing it would add a per-event heap alloc on the hot input path.
    #[allow(clippy::large_enum_variant)]
    enum Sel {
        Msg(Option<Msg>),
        Term(Option<Result<crossterm::event::Event, std::io::Error>>),
    }

    // Msgs produced ahead of time — e.g. a non-paste event drained while
    // coalescing a key burst. Processed before pulling the next event.
    let mut pending_msgs: VecDeque<Msg> = VecDeque::new();

    // Main loop. A fatal error inside the loop is captured here and returned
    // AFTER the orderly-shutdown path below, so a draw failure can't skip MCP
    // child cleanup / pending-save drains (the terminal is still restored by
    // `TerminalGuard::Drop` regardless).
    let mut exit_result: Result<()> = Ok(());
    loop {
        // Render the current state. ratatui's draw closure captures
        // &state, so we don't thread &mut state through the renderer.
        if let Err(err) = terminal
            .as_mut()
            .expect("terminal guard is alive while the render loop runs")
            .inner_mut()
            .draw(|f| render(&state, &mut rstate, f))
        {
            exit_result = Err(err.into());
            break;
        }

        // Drain any msgs queued by a prior burst-coalesce before blocking
        // on the next event.
        let msg = if let Some(queued) = pending_msgs.pop_front() {
            Some(queued)
        } else {
            let selected = tokio::select! {
                // Fair (unbiased) polling. With `biased;`, the hot `msg_rx`
                // arm would always win under sustained streaming and starve
                // terminal input + OS signals (#112). Fair selection still
                // drains streaming promptly — it's almost always ready — while
                // guaranteeing the input/signal/tick arms get serviced too.
                //
                // Effect results (streaming chunks, tool output, …).
                m = msg_rx.recv() => Sel::Msg(m),
                // Crossterm events. Handled below, outside the select!, so
                // coalescing can re-borrow `events`.
                e = events.next() => Sel::Term(e),
                // OS lifecycle signals. A typed Ctrl+C in raw mode is handled
                // by the crossterm branch above; this covers SIGINT/SIGTERM/
                // SIGHUP delivered externally.
                s = lifecycle.next_msg() => Sel::Msg(s),
                // Tick — drives elapsed-time displays + self-dismissing status
                // lines without busy-waiting.
                _ = tick.tick() => Sel::Msg(Some(Msg::Tick)),
            };

            match selected {
                Sel::Msg(m) => m,
                Sel::Term(Some(Ok(evt))) => {
                    if let crossterm::event::Event::Mouse(m) = &evt {
                        use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind as MEK};
                        let ctrl = m.modifiers.contains(KeyModifiers::CONTROL);
                        match m.kind {
                            // F13: Ctrl+Click a chat image tile opens it via
                            // the system viewer. The screen→image mapping
                            // lives in ChatState (the render layer).
                            MEK::Down(MouseButton::Left) if ctrl => rstate
                                .chat
                                .find_image_at_screen_pos(m.row)
                                .map(|target| Msg::OpenImageAt {
                                    message_index: target.message_index,
                                    image_index: target.image_index,
                                }),
                            // Plain (no-modifier) left drag selects chat text.
                            // Handled render-side so wheel-scroll + Ctrl+Click
                            // keep working; on release we copy the selection.
                            MEK::Down(MouseButton::Left) => {
                                rstate.chat.begin_selection(m.row, m.column);
                                None
                            },
                            MEK::Drag(MouseButton::Left) => {
                                rstate.chat.update_selection(m.row, m.column);
                                None
                            },
                            MEK::Up(MouseButton::Left) => {
                                // A drag only *selects* (the highlight persists);
                                // copying is an explicit action (Ctrl+Shift+C).
                                // Auto-copying on release would silently clobber
                                // the user's clipboard.
                                None
                            },
                            MEK::ScrollUp => Some(Msg::MouseScroll {
                                delta: crate::constants::UI_MOUSE_SCROLL_LINES as i16,
                            }),
                            MEK::ScrollDown => Some(Msg::MouseScroll {
                                delta: -(crate::constants::UI_MOUSE_SCROLL_LINES as i16),
                            }),
                            _ => None,
                        }
                    } else {
                        // Non-mouse event. Ctrl+Shift+C copies the current chat
                        // selection — the explicit copy step after a drag-select.
                        // Because the app holds the mouse, the terminal has no
                        // selection of its own and passes the shortcut through.
                        if let crossterm::event::Event::Key(k) = &evt
                            && k.kind == crossterm::event::KeyEventKind::Press
                            && k.modifiers
                                .contains(crossterm::event::KeyModifiers::CONTROL)
                            && k.modifiers.contains(crossterm::event::KeyModifiers::SHIFT)
                            && matches!(k.code, crossterm::event::KeyCode::Char(c) if c.eq_ignore_ascii_case(&'c'))
                        {
                            // Route the copy through the reducer (#18): the
                            // selection lives in the render layer, but emitting a
                            // Msg keeps the clipboard side effect recorded +
                            // replayable instead of dispatched out-of-band.
                            rstate
                                .chat
                                .selected_text()
                                .filter(|t| !t.is_empty())
                                .map(Msg::CopySelection)
                        } else {
                            // Coalesce a paste burst (crossterm 0.29 doesn't
                            // deliver Event::Paste on the Windows console — a
                            // paste arrives as a flood of Char/Enter key events).
                            // The drain pulls every immediately-available event
                            // so the whole block lands as one atomic Msg::Paste.
                            let (primary, trailing) = coalesce_key_burst(evt, || {
                                events.next().now_or_never().flatten().and_then(|r| r.ok())
                            });
                            for queued in trailing {
                                pending_msgs.push_back(queued);
                            }
                            primary
                        }
                    }
                },
                Sel::Term(Some(Err(error))) => {
                    tracing::warn!(error = %error, "terminal event stream failed");
                    None
                },
                Sel::Term(None) => Some(Msg::RuntimeSignal(RuntimeSignal::Hangup)),
            }
        };

        let Some(msg) = msg else { continue };

        // Inject the wall clock as data (Cause 3): one stamp per tick, shared
        // by the recording and the reducer. The recorded `ts` IS the
        // `state.now` this Msg was reduced under, so `--replay` folds the
        // same log by stamping each entry's `ts` here and recomputes the
        // exact same states.
        let now = chrono::Local::now();

        // Optional recording: one JSONL line per Msg, before the
        // reducer runs so the log captures even no-op inputs.
        if let Some(r) = recorder.as_mut()
            && let Err(err) = r.record_msg(now, &msg)
        {
            tracing::warn!(error = %err, "recorder: failed to record message; --replay may be non-deterministic");
        }

        state.now = now;
        let (new_state, cmds) = update(state, msg);
        state = new_state;
        for cmd in cmds {
            runner.dispatch(cmd);
        }

        if state.should_exit {
            break;
        }
    }

    // Seal the recording with a fingerprint of the final session, so a
    // future `--replay` can verify its fold reproduces what this live
    // session actually saw — not merely that the fold is self-consistent.
    // (Wall-clock read is fine here: we're outside the reducer.)
    if let Some(r) = recorder.as_mut()
        && let Err(err) = r.record_trailer(chrono::Local::now(), &state.session)
    {
        tracing::warn!(error = %err, "recorder: failed to write replay trailer");
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

    // Orderly shutdown — wait for any pending saves / scope cleanup. Runs even
    // when the loop broke on a draw error, so MCP children are reaped cleanly.
    runner.shutdown().await;
    exit_result
}

/// Commands dispatched on startup before the first iteration of the
/// loop. Fires MCP init (if configured). Instructions/memory are loaded
/// by the config watcher (#45), not here.
fn bootstrap_cmds(config: &Config) -> Vec<Cmd> {
    // Instructions/memory load + stay fresh via the config watcher (#45),
    // started in `run_interactive_with`. Bootstrap only handles MCP init.
    let mut cmds = Vec::new();
    if !config.mcp_servers.is_empty() {
        cmds.push(Cmd::InitMcpServers(config.mcp_servers.clone()));
    }
    cmds
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_is_empty_without_mcp_servers() {
        // Instructions/memory load via the config watcher (#45), not bootstrap;
        // with no MCP servers configured, bootstrap emits nothing.
        let cmds = bootstrap_cmds(&Config::default());
        assert!(cmds.is_empty());
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
                ..Default::default()
            },
        );
        let cmds = bootstrap_cmds(&cfg);
        assert!(cmds.iter().any(|c| matches!(c, Cmd::InitMcpServers(_))));
    }
}
