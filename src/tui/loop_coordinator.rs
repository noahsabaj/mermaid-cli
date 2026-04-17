//! TUI event loop + the `TuiObserver` that plugs the TUI into the
//! shared agent loop.
//!
//! The observer pattern in `runtime::agent_loop` lets us thread
//! channel-based streaming, live rendering, and queued-message
//! interception through a single shared loop — this module just
//! implements the hooks. The TUI no longer owns a forked `run_agent_loop`.

use anyhow::Result;
use async_trait::async_trait;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};
use ratatui::{Terminal, backend::CrosstermBackend};
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{RwLock, mpsc};

use super::state::{AppState, GenerationStatus};
use super::tui_stream_event::TuiStreamEvent;
use crate::agents::{
    ActionResult as AgentActionResult, AgentAction, SubagentProgress, SubagentResult,
    collect_subagent_results, spawn_subagents,
};
use crate::constants::{UI_MOUSE_SCROLL_LINES, UI_POLL_INTERVAL_MS};
use crate::models::{
    ChatMessage, ErrorCategory, MessageRole, Model, ModelConfig, StreamCallback, StreamEvent,
};
use crate::runtime::agent_loop::{
    AgentObserver, LoopControl, MAX_AGENT_ITERATIONS, ModelCallOutput, run_agent_loop,
};
use crate::tui::App;
use crate::tui::render::render_ui;

/// Import our specialized handlers
use super::action_handler;
use super::command_handler;
use super::event_handler::{EventAction, handle_event};

/// Observer that bridges the shared agent loop to the TUI's `App` and
/// `Terminal`. Owns mutable refs to both for the lifetime of a turn.
///
/// The observer's hooks:
/// - `check_interrupt` drains crossterm events, maps Esc/Ctrl+C to
///   Interrupt and any queued message to InjectMessage.
/// - `on_status` / `on_tool_result` / `on_generation_*` mutate App
///   state and re-render.
/// - `call_model` spawns a model task that streams chunks to a channel,
///   then drains the channel while rendering between ticks.
/// - `run_subagents` spawns subagents and polls their handles with
///   live rendering (mirrors the old `poll_subagent_handles`).
/// - `on_message_appended` mirrors every new message from
///   run_agent_loop into `app.session_state.messages` so the UI sees
///   tool and assistant messages in real time.
pub struct TuiObserver<'a> {
    pub app: &'a mut App,
    pub terminal: &'a mut Terminal<CrosstermBackend<io::Stdout>>,
    pub tx: mpsc::Sender<TuiStreamEvent>,
    pub rx: &'a mut mpsc::Receiver<TuiStreamEvent>,
    /// Set true by `handle_stream_error` once an error has been added to
    /// the chat as a system message. When `run_agent_loop` subsequently
    /// calls `on_error`, we skip adding a second (lower-fidelity) message
    /// since the detailed one is already on screen.
    pub error_already_displayed: bool,
}

impl<'a> TuiObserver<'a> {
    fn render(&mut self) -> Result<()> {
        self.terminal.draw(|f| render_ui(f, self.app))?;
        Ok(())
    }

    /// Drain any pending crossterm events (keys, mouse, paste), apply
    /// their effects to the app, and return whether the user wants to
    /// interrupt. Also handles Enter to queue a message (intercepted
    /// later by `check_interrupt` via `LoopControl::InjectMessage`).
    fn drain_events(&mut self) -> Result<bool> {
        while event::poll(Duration::from_millis(0))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Esc => return Ok(true),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(true);
                    },
                    KeyCode::Char(c)
                        if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
                    {
                        self.app.input.insert(c);
                    },
                    KeyCode::Enter => {
                        if !self.app.input.is_empty() && !self.app.input.get().starts_with('/') {
                            let input = self.app.input.get().to_string();
                            self.app.operation_state.queue_message(input);
                            self.app.clear_input();
                            self.app.set_status("Message queued");
                        }
                    },
                    KeyCode::Backspace => {
                        self.app.input.backspace();
                    },
                    KeyCode::Delete => {
                        self.app.input.delete();
                    },
                    KeyCode::Left => self.app.input.move_left(),
                    KeyCode::Right => self.app.input.move_right(),
                    KeyCode::Home => self.app.input.move_home(),
                    KeyCode::End => self.app.input.move_end(),
                    KeyCode::PageUp => self.app.scroll_up(10),
                    KeyCode::PageDown => self.app.scroll_down(10),
                    _ => {},
                },
                Event::Mouse(m) => match m.kind {
                    MouseEventKind::ScrollUp => self.app.scroll_up(UI_MOUSE_SCROLL_LINES),
                    MouseEventKind::ScrollDown => self.app.scroll_down(UI_MOUSE_SCROLL_LINES),
                    _ => {},
                },
                Event::Paste(text) => {
                    let cleaned = text.replace('\r', "");
                    if !cleaned.is_empty() {
                        self.app.input.insert_str(&cleaned);
                    }
                },
                _ => {},
            }
        }
        Ok(false)
    }

    /// Save partial streamed content as an assistant message (on abort).
    fn commit_partial_response(&mut self) {
        let partial = self.app.take_response();
        if !partial.is_empty() {
            self.app.add_message(MessageRole::Assistant, partial);
        }
    }

    /// Check the rx channel for a fatal error event AND dispatch
    /// capability-disable responses (thinking / vision).
    fn handle_stream_error(&mut self, user_error: &crate::models::UserFacingError) {
        self.app.clear_response();

        // "Does not support thinking" → snap reasoning to None for this
        // model and tell the user. Replaces the legacy
        // `disable_thinking_support` sentinel with a single source of
        // truth on `base_config.reasoning`.
        if user_error.message.contains("does not support thinking") {
            self.app
                .model_state
                .set_reasoning(crate::models::ReasoningLevel::None);
            let _ = crate::app::persist_reasoning_for_model(
                &self.app.model_state.model_id,
                crate::models::ReasoningLevel::None,
            );
            self.app
                .set_status("Model does not support thinking — reasoning set to none");
            self.app.add_message(
                MessageRole::System,
                "This model does not support thinking mode. Reasoning has been set to `none`. Please try your request again."
                    .to_string(),
            );
            return;
        }

        // Vision / image errors → strip images from history and mark vision unsupported.
        let lower = user_error.message.to_lowercase();
        if lower.contains("does not support images")
            || lower.contains("images not supported")
            || lower.contains("does not support vision")
            || lower.contains("is not a multimodal model")
            || lower.contains("unsupported content type")
        {
            self.app.model_state.vision_supported = Some(false);
            for msg in &mut self.app.session_state.messages {
                msg.images = None;
            }
            self.app
                .set_status("Model does not support images - disabled");
            self.app.add_message(
                MessageRole::System,
                "This model does not support images. Image paste has been disabled for this session.".to_string()
            );
            return;
        }

        let status_prefix = match user_error.category {
            ErrorCategory::Connection => "Connection",
            ErrorCategory::Auth => "Auth",
            ErrorCategory::Config => "Config",
            ErrorCategory::NotFound => "Not Found",
            ErrorCategory::Temporary => "Temporary",
            ErrorCategory::Internal => "Error",
        };
        self.app
            .set_status(format!("[{}] {}", status_prefix, user_error.summary));
        let error_display = format!(
            "{}\n\nSuggestion: {}",
            user_error.message, user_error.suggestion
        );
        self.app.add_message(MessageRole::System, error_display);

        // Post-mortem breadcrumb. In-band stream errors previously never
        // hit `tracing` — only startup / auth errors did — so post-failure
        // debugging had to rely on whatever the user could quote from the
        // UI. Log the category, summary, and detail so `~/.mermaid/
        // mermaid.log` captures the incident.
        tracing::warn!(
            category = ?user_error.category,
            summary = %user_error.summary,
            recoverable = user_error.recoverable,
            "stream error: {}",
            user_error.message
        );

        // Mark the error as already surfaced so `on_error` (called later
        // by `run_agent_loop`) doesn't add a second system message with
        // the same content. See `AgentObserver::on_error` override below.
        self.error_already_displayed = true;
    }
}

#[async_trait]
impl<'a> AgentObserver for TuiObserver<'a> {
    fn check_interrupt(&mut self) -> LoopControl {
        // Render first so the user sees the latest state before we decide.
        let _ = self.render();

        // Auto-transition from Sending to Thinking after 1s without chunks.
        if self.app.app_state.generation_status() == Some(GenerationStatus::Sending)
            && let Some(start_time) = self.app.app_state.generation_start_time()
            && start_time.elapsed().as_secs() >= 1
        {
            self.app.transition_to_thinking();
        }

        match self.drain_events() {
            Ok(true) => {
                self.commit_partial_response();
                self.app.set_status("Agent loop interrupted");
                LoopControl::Interrupt
            },
            Ok(false) => {
                if let Some(queued) = self.app.operation_state.take_queued_message() {
                    self.app.set_status("Processing queued message...");
                    LoopControl::InjectMessage(queued)
                } else {
                    LoopControl::Continue
                }
            },
            Err(_) => LoopControl::Continue,
        }
    }

    fn on_status(&mut self, message: &str) {
        self.app.set_status(message);
        let _ = self.render();
    }

    fn on_tool_result(
        &mut self,
        tool_name: &str,
        _tool_call_id: &str,
        action: &AgentAction,
        result: &AgentActionResult,
    ) {
        // Build a rich ActionDisplay and attach it to the last assistant
        // message in session_state for inline rendering under the chat.
        if tool_name == "agent" {
            // Subagent results receive a dedicated display builder that
            // knows how to show tool-use counts, duration, etc. — we
            // don't have access to the SubagentResult here, so build
            // a minimal Success/Error display from the raw result.
            return;
        }
        let action_display = match result {
            AgentActionResult::Success { output, .. } => {
                action_handler::build_action_display(action, output)
            },
            AgentActionResult::Error { error } => {
                action_handler::build_error_display(action, error)
            },
        };
        if let Some(last_msg) = self
            .app
            .session_state
            .messages
            .iter_mut()
            .rev()
            .find(|m| matches!(m.role, MessageRole::Assistant))
        {
            last_msg.actions.push(action_display);
        }
        let _ = self.render();
    }

    fn on_error(&mut self, error: &str) {
        // `run_agent_loop` calls this when `call_model` returns Err. For
        // stream errors that arrived via `TuiStreamEvent::Error`, the
        // message was already added to the chat by `handle_stream_error`
        // — so here we only need to refresh the status (and skip adding
        // a duplicate system message with the bail-summary string).
        //
        // For other error paths (channel closed unexpectedly, user Esc
        // → "Generation interrupted", spurious errors outside the stream
        // pipeline), the flag is still false and we fall back to the
        // legacy `display_error_simple` behavior so the user still sees
        // something meaningful.
        if self.error_already_displayed {
            self.app.set_status(format!("[Error] {}", error));
            self.error_already_displayed = false;
        } else {
            self.app.display_error_simple(error);
        }
        let _ = self.render();
    }

    fn on_generation_start(&mut self) {
        // Streaming state is already set up by `call_model` when it
        // spawns the model task; nothing else needed here.
    }

    fn on_generation_complete(&mut self, tokens: usize) {
        self.app.set_final_tokens(tokens);
    }

    fn on_message_appended(&mut self, msg: &ChatMessage) {
        // Mirror the message into session_state so the UI renders it
        // live. The agent-loop's own `messages` vec (which is a
        // per-call snapshot — see `run_agent_loop_for_turn`) is
        // independent.
        self.app.commit_message(msg.clone());
    }

    async fn call_model(
        &mut self,
        model: Arc<RwLock<Box<dyn Model>>>,
        _messages: &[ChatMessage],
        config: &ModelConfig,
    ) -> Result<ModelCallOutput> {
        // Step 5h: auto-reload MERMAID.md if it changed since last turn.
        // mtime poll is microseconds; we only re-read the file when the
        // mtime actually moved. Status messages surface visible changes
        // so users know context shifted under them.
        {
            use crate::app::instructions::{ReloadOutcome, refresh};
            let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            let prior = self.app.instructions.take();
            let (refreshed, outcome) = refresh(prior, &cwd);
            self.app.instructions = refreshed;
            match outcome {
                ReloadOutcome::Reloaded {
                    old_tokens,
                    new_tokens,
                } => {
                    self.app.set_status(format!(
                        "MERMAID.md updated — reloaded ({} → {} tokens)",
                        old_tokens, new_tokens
                    ));
                },
                ReloadOutcome::LoadedFirst { tokens } => {
                    self.app
                        .set_status(format!("MERMAID.md created — loaded ({} tokens)", tokens));
                },
                ReloadOutcome::Removed => {
                    self.app
                        .set_status("MERMAID.md removed — context refreshed");
                },
                ReloadOutcome::Unchanged => {},
            }
        }

        // Build the trimmed window from session_state (source of truth).
        // The `_messages` param is the per-call snapshot maintained by
        // run_agent_loop; for the TUI we always re-trim from the full
        // session_state because tool results have been mirrored in.
        let trimmed = self.app.build_managed_message_history(
            crate::constants::MAX_CONTEXT_TOKENS,
            crate::constants::CONTEXT_RESERVE_TOKENS,
        );

        // Per-call resets
        self.app.operation_state.accumulated_tool_calls.clear();
        self.app.clear_response();

        let tx_stream = self.tx.clone();
        let tx_done = self.tx.clone();
        let model_clone = Arc::clone(&model);

        // When reasoning is set to None we suppress reasoning chunks at
        // the stream level too, so the adapter doesn't waste work emitting
        // tokens we'd turn around and drop. Independent of the explicit
        // `hide_reasoning_trace` toggle (a user can hide the trace while
        // keeping reasoning depth high).
        let mut effective_config = config.clone();
        let reasoning_off = config.reasoning == crate::models::ReasoningLevel::None;
        effective_config.hide_reasoning_trace = config.hide_reasoning_trace || reasoning_off;
        effective_config.dynamic_system_suffix =
            self.app.instructions.as_ref().map(|i| i.content.clone());

        let handle = tokio::spawn(async move {
            // Track entry/exit of reasoning phase so the TUI's existing
            // marker-based rendering keeps working unchanged. Wave 4
            // (renaming TuiStreamEvent) deliberately stayed name-only;
            // proper reasoning rendering ships in Step 4 of the
            // multi-provider rollout.
            let in_thinking = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let sent_text = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let in_thinking_cb = Arc::clone(&in_thinking);
            let sent_text_cb = Arc::clone(&sent_text);
            let tx_stream_cb = tx_stream.clone();

            let typed_callback: StreamCallback = Arc::new(move |event| match event {
                StreamEvent::Text(text) => {
                    if in_thinking_cb.swap(false, std::sync::atomic::Ordering::Relaxed) {
                        let _ = tx_stream_cb
                            .try_send(TuiStreamEvent::Chunk("\n...done thinking.\n\n".to_string()));
                    }
                    sent_text_cb.store(true, std::sync::atomic::Ordering::Relaxed);
                    let _ = tx_stream_cb.try_send(TuiStreamEvent::Chunk(text));
                },
                StreamEvent::Reasoning(chunk) => {
                    if !in_thinking_cb.swap(true, std::sync::atomic::Ordering::Relaxed) {
                        let _ = tx_stream_cb
                            .try_send(TuiStreamEvent::Chunk("Thinking...\n\n".to_string()));
                    }
                    if !chunk.text.is_empty() {
                        sent_text_cb.store(true, std::sync::atomic::Ordering::Relaxed);
                        let _ = tx_stream_cb.try_send(TuiStreamEvent::Chunk(chunk.text));
                    }
                },
                StreamEvent::ToolCall(tc) => {
                    let _ = tx_stream_cb.try_send(TuiStreamEvent::ToolCalls(vec![tc]));
                },
                // Done is captured below via `response.usage` and re-emitted
                // AFTER any defensive-fallback chunk so consumer ordering
                // matches the pre-typed-events behavior.
                StreamEvent::Done { .. } => {},
            });

            let model = model_clone.read().await;
            match model
                .chat(&trimmed, &effective_config, Some(typed_callback))
                .await
            {
                Ok(response) => {
                    // Defensive fallback: forward buffered content if the
                    // adapter didn't stream anything (preserves legacy
                    // behavior; adapters shouldn't hit this in practice).
                    if !sent_text.load(std::sync::atomic::Ordering::Relaxed)
                        && !response.content.is_empty()
                    {
                        let _ = tx_done
                            .send(TuiStreamEvent::Chunk(response.content.clone()))
                            .await;
                    }
                    // Tool calls: emit anything the response carries that
                    // the typed stream might have missed (Ollama's own
                    // chat_typed emits them as events, so this is usually
                    // empty — but the receiver dedupes by accumulating).
                    if let Some(ref tool_calls) = response.tool_calls
                        && !tool_calls.is_empty()
                    {
                        let _ = tx_done
                            .send(TuiStreamEvent::ToolCalls(tool_calls.clone()))
                            .await;
                    }
                    let tokens = response.usage.map(|u| u.total_tokens).unwrap_or(0);
                    let _ = tx_done
                        .send(TuiStreamEvent::Done {
                            total_tokens: tokens,
                        })
                        .await;
                },
                Err(e) => {
                    let _ = tx_done
                        .send(TuiStreamEvent::Error(e.to_user_facing()))
                        .await;
                },
            }
        });

        // Install abort handle into app_state so Esc can cancel.
        if self.app.app_state.is_generating() {
            self.app.update_abort_handle(handle.abort_handle());
            self.app.transition_to_sending();
        } else {
            self.app.start_generation(handle.abort_handle());
        }

        // Drain the channel. Render between events and honor the full
        // input handler so the user can type, edit, scroll, and queue
        // messages mid-stream (queued messages are consumed by
        // `check_interrupt` between agent-loop steps). `drain_events`
        // inserts chars into `app.input`, handles Enter-to-queue,
        // backspace/arrow navigation, PageUp/PageDown, mouse scroll,
        // and returns `true` on Esc / Ctrl+C.
        loop {
            let _ = self.render();

            if self.drain_events()? {
                let (abort, partial) = self.app.abort_generation();
                if let Some(h) = abort {
                    h.abort();
                }
                if !partial.is_empty() {
                    self.app.add_message(MessageRole::Assistant, partial);
                }
                self.app.set_status("Generation stopped");
                anyhow::bail!("Generation interrupted by user");
            }

            // Try to receive next event with a short timeout so we
            // don't busy-spin.
            let event = match tokio::time::timeout(
                Duration::from_millis(UI_POLL_INTERVAL_MS),
                self.rx.recv(),
            )
            .await
            {
                Ok(Some(ev)) => ev,
                Ok(None) => anyhow::bail!("Stream channel closed unexpectedly"),
                Err(_) => continue, // timeout — loop and render again
            };

            match event {
                TuiStreamEvent::Chunk(text) => {
                    self.app.push_response(&text);
                    if self.app.app_state.generation_status() != Some(GenerationStatus::Streaming) {
                        self.app.transition_to_streaming();
                    }
                },
                TuiStreamEvent::ToolCalls(calls) => {
                    self.app
                        .operation_state
                        .accumulated_tool_calls
                        .extend(calls);
                },
                TuiStreamEvent::Done { total_tokens } => {
                    // Extract buffered content + accumulated tool_calls.
                    let content = self.app.take_response();
                    let tool_calls =
                        std::mem::take(&mut self.app.operation_state.accumulated_tool_calls);
                    return Ok(ModelCallOutput {
                        content,
                        tool_calls,
                        tokens: total_tokens,
                    });
                },
                TuiStreamEvent::Error(err) => {
                    self.handle_stream_error(&err);
                    anyhow::bail!("{}", err.summary);
                },
            }
        }
    }

    async fn run_subagents(
        &mut self,
        specs: Vec<(String, String)>,
        model: Arc<RwLock<Box<dyn Model>>>,
        config: &ModelConfig,
    ) -> Vec<SubagentResult> {
        let progress = Arc::new(Mutex::new(Vec::<SubagentProgress>::new()));
        self.app.operation_state.active_subagents = Some(Arc::clone(&progress));

        let (handles, overflow) = spawn_subagents(specs, model, config, Arc::clone(&progress));

        let handle_count = handles.len();
        self.app
            .set_status(format!("Running {} agent(s)...", handle_count));

        // Poll handles with rendering between checks.
        loop {
            // Sync token count from subagents for the status line.
            if let Some(ref p) = self.app.operation_state.active_subagents {
                let total_tokens: usize = p
                    .lock()
                    .map(|v| v.iter().map(|a| a.tokens).sum())
                    .unwrap_or(0);
                if let AppState::Generating {
                    tokens_received, ..
                } = &mut self.app.app_state
                {
                    *tokens_received = total_tokens;
                }
            }

            let _ = self.render();

            // Esc aborts all subagent handles and returns an empty result.
            let mut interrupt = false;
            if let Ok(true) = self.drain_events() {
                interrupt = true;
            }
            if interrupt {
                for h in &handles {
                    h.abort();
                }
                self.app.operation_state.clear_subagents();
                return Vec::new();
            }

            if handles.iter().all(|h| h.is_finished()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(UI_POLL_INTERVAL_MS)).await;
        }

        let results = collect_subagent_results(handles, overflow).await;

        // Attach a completion display for each subagent to the last
        // assistant message so they render inline like tool actions.
        for result in &results {
            let display = action_handler::build_agent_action_display(result);
            if let Some(last_msg) = self
                .app
                .session_state
                .messages
                .iter_mut()
                .rev()
                .find(|m| matches!(m.role, MessageRole::Assistant))
            {
                last_msg.actions.push(display);
            }
            self.app.session_state.cumulative_tokens += result.tokens;
        }

        self.app
            .set_status(format!("{} agent(s) completed", results.len()));
        self.app.operation_state.clear_subagents();
        results
    }
}

/// Run the main application event loop.
///
/// Top-level: handles UI rendering, non-generation keyboard events,
/// command dispatch, and kicks off model turns. When a model turn
/// starts it builds a `TuiObserver`, makes the first model call, and
/// hands control to `run_agent_loop` for tool execution — the TUI
/// observer's hooks render, stream, and intercept queued messages
/// inside the shared loop.
pub async fn run_app_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    tx: mpsc::Sender<TuiStreamEvent>,
    rx: &mut mpsc::Receiver<TuiStreamEvent>,
) -> Result<()> {
    let mut title_task: Option<tokio::task::JoinHandle<Option<String>>> = None;

    loop {
        // Poll background title generation.
        if let Some(ref task) = title_task
            && task.is_finished()
            && let Some(task) = title_task.take()
            && let Ok(Some(title)) = task.await
        {
            app.session_state.conversation_title = Some(title);
        }

        // Poll background MCP init.
        app.poll_mcp_init().await;

        let viewport_height = terminal.size()?.height.saturating_sub(8);
        terminal.draw(|f| render_ui(f, app))?;

        // Idle: poll keyboard/mouse events.
        if event::poll(Duration::from_millis(UI_POLL_INTERVAL_MS))? {
            let event = event::read()?;
            match handle_event(app, event, viewport_height)? {
                EventAction::Continue => {},
                EventAction::Quit => break,
                EventAction::SubmitMessage(input) => {
                    run_turn(app, input, terminal, &tx, rx).await?;

                    // Process any messages queued while the turn was running.
                    while let Some(queued) = app.operation_state.take_queued_message() {
                        run_turn(app, queued, terminal, &tx, rx).await?;
                    }

                    // Turn complete — spawn title generation if first one.
                    app.stop_generation();
                    if title_task.is_none() {
                        title_task = app.spawn_title_generation();
                    }
                },
                EventAction::ExecuteCommand(command) => {
                    command_handler::handle_command(app, terminal, &command).await?;
                },
            }
        }

        if !app.running {
            break;
        }
    }

    Ok(())
}

/// Drive a single "turn": commit user input, call the model once,
/// then delegate to `run_agent_loop` for tool execution if any.
async fn run_turn(
    app: &mut App,
    input: String,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    tx: &mpsc::Sender<TuiStreamEvent>,
    rx: &mut mpsc::Receiver<TuiStreamEvent>,
) -> Result<()> {
    // Take any attached images before adding message
    let images = app.attachment_state.take_base64_data();
    app.add_message_with_images(MessageRole::User, input.clone(), images);

    // Save to input history (capped + dedup).
    app.input.add_to_history(input.clone());
    app.input.history_index = None;
    app.input.history_buffer.clear();

    // Persist the conversation asynchronously.
    if let Some(ref mut conv) = app.session_state.current_conversation {
        conv.add_to_input_history(input);
        if let Some(ref manager) = app.session_state.conversation_manager {
            let conv_clone = conv.clone();
            let manager_clone = manager.clone();
            tokio::task::spawn_blocking(move || {
                let _ = manager_clone.save_conversation(&conv_clone);
            });
        }
    }

    // Make the first model call via the observer, then enter the
    // shared agent loop with whatever tool_calls it produced.
    let mut observer = TuiObserver {
        app,
        terminal,
        tx: tx.clone(),
        rx,
        error_already_displayed: false,
    };
    let model = Arc::clone(&observer.app.model_state.model);
    let config = observer.app.build_model_config();

    observer.on_generation_start();
    let first_call = observer.call_model(Arc::clone(&model), &[], &config).await;
    let (content, initial_tool_calls) = match first_call {
        Ok(out) => {
            observer.on_generation_complete(out.tokens);
            (out.content, out.tool_calls)
        },
        Err(_) => {
            // Error already handled inside call_model (status + system message).
            observer.app.stop_generation();
            return Ok(());
        },
    };

    // Commit the assistant message (with tool_calls, if any).
    if !content.is_empty() || !initial_tool_calls.is_empty() {
        observer
            .app
            .add_assistant_message_with_tool_calls(content, initial_tool_calls.clone());
    }

    // If the model asked for tools, run the agent loop.
    if !initial_tool_calls.is_empty() {
        // The shared loop owns a local `messages` vec so it doesn't
        // alias session_state.messages (which the observer mirrors
        // into via on_message_appended).
        let mut messages = observer.app.build_managed_message_history(
            crate::constants::MAX_CONTEXT_TOKENS,
            crate::constants::CONTEXT_RESERVE_TOKENS,
        );
        let _ = run_agent_loop(
            model,
            &config,
            &mut messages,
            initial_tool_calls,
            &mut observer,
            MAX_AGENT_ITERATIONS,
        )
        .await?;
    }

    Ok(())
}
