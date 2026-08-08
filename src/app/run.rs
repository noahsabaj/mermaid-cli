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
use ratatui::layout::Rect;
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
#[expect(
    clippy::too_many_lines,
    reason = "predates the lint; see .github/baselines/expect_budget.txt"
)]
pub async fn run_interactive_with(
    mut config: Config,
    cwd: PathBuf,
    model_id: String,
    mut opts: InteractiveOptions,
) -> Result<()> {
    // One startup clock read, shared by `State::new` and the recording
    // header: replay seeds `State::new` with the recorded value and gets the
    // same initial conversation id/title.
    let startup_now = chrono::Local::now();
    // Fold enabled plugins' MCP servers + agent types into the merged config
    // BEFORE anything consumes it (State::new seeds server rows, the
    // recording header captures the merged config — replay-faithful, and the
    // provider factory + tool registry see the same view).
    let plugin_assets = crate::app::plugin_assets::load();
    let plugin_warnings = crate::app::plugin_assets::apply(&mut config, &plugin_assets);
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
    crate::app::stamp_session_provenance(&mut state, &cwd);
    // NO_COLOR (https://no-color.org): present and non-empty disables all
    // color. Read once here — the reducer never touches the environment; the
    // render layer resolves `Theme::plain()` off this flag.
    state.ui.no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
    // Skills load once at startup (authored artifacts, no watcher); the config
    // watcher below keeps only instructions/memory fresh.
    state.skills = crate::app::skills::load(&cwd);
    // Plugin prompt commands: same restart-to-refresh policy as skills.
    state.plugin_commands = plugin_assets.commands;
    for warning in plugin_warnings {
        state
            .ui
            .pending_msgs
            .push_back(Msg::TransientStatus { text: warning });
    }
    let providers = std::sync::Arc::new(crate::providers::ProviderFactory::new(config.clone()));
    let tools = ToolRegistry::build(
        &config,
        crate::providers::TuiMode::Interactive,
        providers.clone(),
    );
    if let Some(capabilities) = tools.web_capabilities()
        && let Some(text) = web_capabilities_notice(&config, capabilities)
    {
        state
            .ui
            .pending_msgs
            .push_back(Msg::TransientStatus { text });
    }
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
    // `Option` because the $EDITOR compose round-trip must DROP the stream
    // (its reader thread holds crossterm's internal reader mutex) before
    // suspending, and build a fresh one after — same lifecycle dance as
    // `terminal` above.
    let mut events = Some(EventStream::new());
    let mut lifecycle = RuntimeLifecycle::new();
    let mut tick = interval(Duration::from_millis(16));
    let mut recorder = opts.recorder;

    // Boot effects: MCP server init (if configured). Instructions/memory are
    // loaded by the config watcher started above (#45), not here.
    for cmd in bootstrap_cmds(&config, &state.session.conversation.id) {
        runner.dispatch(cmd);
    }
    // A resumed session may carry an in-flight checklist; hand it to the
    // TaskBroker (tool-side truth) so the first task tool call of the new
    // process starts from the restored list instead of an empty one.
    if !state.session.conversation.tasks.tasks.is_empty() {
        runner.dispatch(crate::domain::Cmd::SyncTaskStore(
            state.session.conversation.tasks.clone(),
        ));
    }

    // Which `select!` arm fired. Terminal events are handled *after* the
    // select! returns so the paste-coalescing drain can borrow `events`
    // again without tripping the borrow checker.
    //
    // `Msg` is the large variant, but this enum lives on the stack for one
    // loop iteration and `Msg` is passed by value everywhere already —
    // boxing it would add a per-event heap alloc on the hot input path.
    #[expect(clippy::large_enum_variant)]
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
    // Last-seen `full_redraw_seq`. When the reducer bumps it (shell command
    // finished, Ctrl+L), `Terminal::clear()` resets ratatui's back buffer so
    // the next draw repaints every cell — the only way to overwrite bytes
    // some other process wrote directly to the tty (ghost cells).
    let mut seen_redraw_seq = state.ui.full_redraw_seq;
    loop {
        // Render the current state. ratatui's draw closure captures
        // &state, so we don't thread &mut state through the renderer.
        {
            let term = terminal
                .as_mut()
                .expect("terminal guard is alive while the render loop runs")
                .inner_mut();
            if state.ui.full_redraw_seq != seen_redraw_seq {
                seen_redraw_seq = state.ui.full_redraw_seq;
                // NOT `Terminal::clear()`: it snapshots the cursor with an
                // ESC[6n round-trip, and the reply never arrives — the
                // `EventStream` reader thread is parked holding crossterm's
                // internal reader mutex and swallows it — so the query dies
                // fatally after crossterm's 2s deadline. `resize()` to the
                // current size performs the same full clear + back-buffer
                // reset for a Fullscreen viewport without querying the tty.
                let repaint = term
                    .size()
                    .and_then(|size| term.resize(Rect::new(0, 0, size.width, size.height)));
                if let Err(err) = repaint {
                    exit_result = Err(err.into());
                    break;
                }
            }
            if let Err(err) = term.draw(|f| render(&state, &mut rstate, f)) {
                exit_result = Err(err.into());
                break;
            }
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
                e = events.as_mut().expect("event stream is alive while the loop runs").next() => Sel::Term(e),
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
                                    image_number: target.image_number,
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
                        // The SHIFT bit only arrives when the kitty keyboard
                        // protocol was negotiated at setup (TerminalGuard); on
                        // legacy terminals Ctrl+Shift+C is transmitted as the
                        // identical byte 0x03 as Ctrl+C — physically
                        // indistinguishable — so there it falls through to the
                        // reducer's Ctrl+C handling (press-twice-to-exit keeps
                        // a stray copy-chord harmless).
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
                                events
                                    .as_mut()
                                    .expect("event stream is alive while the loop runs")
                                    .next()
                                    .now_or_never()
                                    .flatten()
                                    .and_then(|r| r.ok())
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
        // `ComposeInEditor` is run-loop-owned (it suspends the terminal +
        // event stream, which only this loop holds); everything else goes to
        // the effect runner. At most one compose per reducer step by
        // construction (single Ctrl+O / /editor arm).
        let mut compose_draft: Option<String> = None;
        for cmd in cmds {
            if let Cmd::ComposeInEditor { text } = cmd {
                compose_draft = Some(text);
            } else {
                runner.dispatch(cmd);
            }
        }
        if let Some(draft) = compose_draft {
            match crate::app::editor::compose_in_editor(&mut terminal, &mut events, draft).await {
                // Through pending_msgs, so the result flows through the
                // recorder like any input — --replay never launches an editor.
                Ok(msg) => pending_msgs.push_back(msg),
                Err(err) => {
                    exit_result = Err(err);
                    break;
                },
            }
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
/// loop. Fires MCP init (if configured) and materializes the session's
/// scratch directory. Instructions/memory are loaded by the config
/// watcher (#45), not here.
fn bootstrap_cmds(config: &Config, session_id: &str) -> Vec<Cmd> {
    // Instructions/memory load + stay fresh via the config watcher (#45),
    // started in `run_interactive_with`.
    let mut cmds = Vec::new();
    if !config.mcp_servers.is_empty() {
        cmds.push(Cmd::InitMcpServers(config.mcp_servers.clone()));
    }
    // Every session gets a scratch dir — `session_id` is captured AFTER any
    // `--continue`/`--resume` seed, so a resumed session adopts the dir
    // keyed by its restored conversation id.
    cmds.push(Cmd::EnsureScratchpad {
        session_id: session_id.to_string(),
    });
    cmds
}

/// One startup-visible summary built from the exact capability resolution used
/// by the registry and subagents. This makes backend/trust routing explicit in
/// the TUI without re-reading credentials or probing platform viability.
///
/// Returns `None` only for the boring case — every capability resolved AND
/// every one of them terminates on this machine — so a healthy sovereign
/// startup stays quiet. Silence therefore means "working and local"; anything
/// else speaks. Availability alone is deliberately NOT the gate: a working
/// cloud backend is exactly what a user needs told, so gating on viability
/// would mute the disclosure precisely when traffic is leaving the machine.
fn web_capabilities_notice(
    config: &Config,
    capabilities: &crate::providers::tool::web::WebCapabilities,
) -> Option<String> {
    use crate::providers::tool::web::Egress;

    if config.safety.network == crate::app::NetworkPolicy::Deny {
        return Some(format!(
            "Web egress disabled by safety.network = \"deny\" (selected fetch backend: {}; selected search backend: {}).",
            capabilities.fetch.backend, capabilities.search.backend
        ));
    }

    let all = [
        ("fetch", &capabilities.fetch),
        ("search", &capabilities.search),
    ];
    let degraded = all
        .into_iter()
        .filter(|(_, status)| !status.available)
        .collect::<Vec<_>>();
    let leaves_machine = all
        .iter()
        .any(|(_, status)| status.egress == Egress::OffMachine);
    if degraded.is_empty() && !leaves_machine {
        return None;
    }

    // Headline stays one line per capability: backend + availability, and the
    // trust destination ONLY where it means something. An unavailable backend
    // routes nowhere, so naming its destination there is noise that also
    // strands the remediation text mid-sentence.
    let headline = |name: &str, status: &crate::providers::tool::web::WebCapabilityStatus| {
        if status.available {
            format!(
                "{name}: {} (available; {})",
                status.backend, status.trust_destination
            )
        } else {
            format!("{name}: {} (unavailable)", status.backend)
        }
    };

    // Remediation prose is a paragraph, not a parenthetical — give each
    // degraded capability its own line below the headline. The marker is a
    // `-` bullet, not leading whitespace: the transcript renderer re-wraps
    // system notices word by word (`wrap_text_with_indent`), so an indent is
    // dropped and the detail lines would be indistinguishable from the
    // wrapped headline. A glyph is a word, so it survives.
    let mut notice = format!(
        "Web capabilities - {}; {}.",
        headline("fetch", &capabilities.fetch),
        headline("search", &capabilities.search)
    );
    for (name, status) in degraded {
        let reason = status
            .reason
            .as_deref()
            .map(crate::utils::redact_secrets)
            .unwrap_or_else(|| "backend initialization failed".to_string());
        let reason = reason.split_whitespace().collect::<Vec<_>>().join(" ");
        let reason = crate::utils::truncate_middle_bytes(&reason, 240)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        notice.push_str(&format!("\n- {name}: {reason}"));
    }
    Some(notice)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_always_ensures_the_session_scratchpad() {
        // Instructions/memory load via the config watcher (#45), not
        // bootstrap; with no MCP servers configured, only the scratchpad
        // ensure remains — keyed by the caller's session id.
        let cmds = bootstrap_cmds(&Config::default(), "sess-1");
        assert_eq!(cmds.len(), 1);
        assert!(
            cmds.iter().any(
                |c| matches!(c, Cmd::EnsureScratchpad { session_id } if session_id == "sess-1")
            )
        );
    }

    #[test]
    fn bootstrap_skips_mcp_init_when_no_servers_configured() {
        let cmds = bootstrap_cmds(&Config::default(), "sess-1");
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
        let cmds = bootstrap_cmds(&cfg, "sess-1");
        assert!(cmds.iter().any(|c| matches!(c, Cmd::InitMcpServers(_))));
    }

    /// Statuses are built by hand rather than via `WebCapabilities::resolve`
    /// so the notice's formatting is asserted independently of whichever
    /// backends happen to be viable on the test host.
    fn capabilities(
        fetch: crate::providers::tool::web::WebCapabilityStatus,
        search: crate::providers::tool::web::WebCapabilityStatus,
    ) -> crate::providers::tool::web::WebCapabilities {
        crate::providers::tool::web::WebCapabilities::from_statuses_for_test(fetch, search)
    }

    fn available(
        backend: &'static str,
        trust_destination: &'static str,
        egress: crate::providers::tool::web::Egress,
    ) -> crate::providers::tool::web::WebCapabilityStatus {
        crate::providers::tool::web::WebCapabilityStatus {
            available: true,
            backend,
            trust_destination,
            egress,
            reason: None,
        }
    }

    fn unavailable(
        backend: &'static str,
        trust_destination: &'static str,
        egress: crate::providers::tool::web::Egress,
        reason: &str,
    ) -> crate::providers::tool::web::WebCapabilityStatus {
        crate::providers::tool::web::WebCapabilityStatus {
            available: false,
            backend,
            trust_destination,
            egress,
            reason: Some(reason.to_string()),
        }
    }

    /// The two sovereign defaults, spelled once: fetch straight off this
    /// machine, search via the locally managed SearXNG process.
    fn local_fetch() -> crate::providers::tool::web::WebCapabilityStatus {
        available(
            "native",
            "direct from this machine",
            crate::providers::tool::web::Egress::OnMachine,
        )
    }

    fn local_search() -> crate::providers::tool::web::WebCapabilityStatus {
        available(
            "managed_searxng",
            "local managed process",
            crate::providers::tool::web::Egress::OnMachine,
        )
    }

    #[test]
    fn web_capability_notice_stays_silent_when_everything_resolved_and_local() {
        let config = Config::default();
        let capabilities = capabilities(local_fetch(), local_search());
        assert_eq!(web_capabilities_notice(&config, &capabilities), None);
    }

    /// The regression this gate exists to prevent: a WORKING cloud backend is
    /// the case a sovereignty-focused tool most needs to disclose, so
    /// viability alone must never buy silence.
    #[test]
    fn web_capability_notice_discloses_working_cloud_egress() {
        let config = Config::default();
        let capabilities = capabilities(
            local_fetch(),
            available(
                "ollama_cloud",
                "Ollama Cloud",
                crate::providers::tool::web::Egress::OffMachine,
            ),
        );
        let notice =
            web_capabilities_notice(&config, &capabilities).expect("cloud egress must disclose");
        assert!(
            notice.contains("search: ollama_cloud (available; Ollama Cloud)"),
            "{notice}"
        );
        // Nothing is broken, so nothing earns a remediation line.
        assert!(!notice.contains('\n'), "{notice}");
    }

    /// An operator-supplied SearXNG URL cannot be proven to be loopback, so it
    /// discloses like any other off-machine destination.
    #[test]
    fn web_capability_notice_discloses_configured_searxng_endpoint() {
        let config = Config::default();
        let capabilities = capabilities(
            local_fetch(),
            available(
                "searxng",
                "configured SearXNG instance",
                crate::providers::tool::web::Egress::OffMachine,
            ),
        );
        let notice =
            web_capabilities_notice(&config, &capabilities).expect("configured endpoint discloses");
        assert!(notice.contains("configured SearXNG instance"), "{notice}");
    }

    #[test]
    fn web_capability_notice_gives_every_degraded_capability_its_own_line() {
        let config = Config::default();
        let capabilities = capabilities(
            unavailable(
                "native",
                "direct from this machine",
                crate::providers::tool::web::Egress::OnMachine,
                "TLS backend failed to initialize",
            ),
            unavailable(
                "managed_searxng",
                "local managed process",
                crate::providers::tool::web::Egress::OnMachine,
                "no sovereign SearXNG bundle is available for this platform",
            ),
        );
        let notice = web_capabilities_notice(&config, &capabilities).expect("both degraded");
        let lines = notice.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 3, "{notice}");
        assert!(lines[1].starts_with("- fetch: TLS backend"), "{notice}");
        assert!(lines[2].starts_with("- search: no sovereign"), "{notice}");
    }

    #[test]
    fn web_capability_notice_discloses_shared_backend_and_trust_routing() {
        let config = Config::default();
        let capabilities = capabilities(
            local_fetch(),
            unavailable(
                "managed_searxng",
                "local managed process",
                crate::providers::tool::web::Egress::OnMachine,
                "no sovereign SearXNG bundle is available for this platform",
            ),
        );
        let notice = web_capabilities_notice(&config, &capabilities).expect("degraded search");
        // The healthy capability still discloses where its traffic goes.
        assert!(notice.contains("fetch: native (available"), "{notice}");
        assert!(notice.contains("direct from this machine"), "{notice}");
        assert!(
            notice.contains("search: managed_searxng (unavailable)"),
            "{notice}"
        );
    }

    #[test]
    fn web_capability_notice_moves_remediation_off_the_headline() {
        let config = Config::default();
        let capabilities = capabilities(
            local_fetch(),
            unavailable(
                "managed_searxng",
                "local managed process",
                crate::providers::tool::web::Egress::OnMachine,
                "no sovereign SearXNG bundle is available for this platform (windows/x86_64).\n  Configure `[web] search_backend = \"ollama\"`.",
            ),
        );
        let notice = web_capabilities_notice(&config, &capabilities).expect("degraded search");
        let (headline, detail) = notice.split_once('\n').expect("detail line");
        // The unavailable backend routes nowhere, so its trust destination is
        // not named — and the paragraph never lands mid-parenthetical.
        assert!(!headline.contains("local managed process"), "{headline}");
        assert!(!headline.contains("SearXNG bundle"), "{headline}");
        assert_eq!(
            detail,
            "- search: no sovereign SearXNG bundle is available for this platform (windows/x86_64). Configure `[web] search_backend = \"ollama\"`."
        );
    }

    #[test]
    fn web_capability_notice_honors_global_network_denial() {
        let mut config = Config::default();
        config.safety.network = crate::app::NetworkPolicy::Deny;
        // Denial reports regardless of viability or locality — both backends
        // resolve here, and both stay on this machine.
        let capabilities = capabilities(local_fetch(), local_search());
        let notice = web_capabilities_notice(&config, &capabilities).expect("denial always shows");
        assert!(notice.contains("Web egress disabled"), "{notice}");
        assert!(notice.contains("fetch backend: native"), "{notice}");
        assert!(
            notice.contains("search backend: managed_searxng"),
            "{notice}"
        );
    }
}
