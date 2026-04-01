use anyhow::Result;
use crossterm::event;
use ratatui::{Terminal, backend::CrosstermBackend};
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

use super::state::GenerationStatus;
use super::stream_event::StreamEvent;
use crate::agents::{
    AgentAction, SubagentProgress, SubagentResult, collect_subagent_results,
    format_subagent_tool_result, spawn_subagents,
};
use crate::constants::UI_POLL_INTERVAL_MS;
use crate::models::{ChatMessage, MessageRole, StreamCallback};
use crate::runtime::agent_loop::{MAX_AGENT_ITERATIONS, ToolExecutionResult};
use crate::tui::App;
use crate::tui::render::render_ui;

/// Import our specialized handlers
use super::action_handler;
use super::command_handler;
use super::event_handler::{EventAction, handle_event};
use super::stream_handler::{StreamStatus, process_stream_chunks};

/// Spawn an async task to stream the model's response and return the abort handle.
///
/// This is the single place that builds the callback, calls the model, and
/// sends [DONE]/[ERROR_JSON] on the channel. All three call-model sites in
/// the event loop delegate here.
fn spawn_model_call(app: &mut App, messages: Vec<ChatMessage>, tx: &mpsc::Sender<StreamEvent>) {
    // Per-model-call resets (these are per-call, not per-turn)
    app.operation_state.accumulated_tool_calls.clear();
    app.clear_response();

    let model = app.model_state.model.clone();
    let tx_stream = tx.clone();
    let tx_done = tx.clone();
    let config = app.build_model_config();

    let handle = tokio::spawn(async move {
        let sent_chunks = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let sent_flag = Arc::clone(&sent_chunks);
        let callback: StreamCallback = Arc::new(move |chunk| {
            sent_flag.store(true, std::sync::atomic::Ordering::Relaxed);
            let _ = tx_stream.try_send(StreamEvent::Chunk(chunk.to_string()));
        });

        let model = model.read().await;
        match model.chat(&messages, &config, Some(callback)).await {
            Ok(response) => {
                // Defensive fallback: if no chunks were streamed but response has
                // content, forward it now. Matches runtime::agent_loop's fallback.
                if !sent_chunks.load(std::sync::atomic::Ordering::Relaxed)
                    && !response.content.is_empty()
                {
                    let _ = tx_done
                        .send(StreamEvent::Chunk(response.content.clone()))
                        .await;
                }
                if let Some(ref tool_calls) = response.tool_calls
                    && !tool_calls.is_empty()
                {
                    let _ = tx_done
                        .send(StreamEvent::ToolCalls(tool_calls.clone()))
                        .await;
                }
                let tokens = response.usage.map(|u| u.total_tokens).unwrap_or(0);
                let _ = tx_done
                    .send(StreamEvent::Done {
                        total_tokens: tokens,
                    })
                    .await;
            },
            Err(e) => {
                let _ = tx_done.send(StreamEvent::Error(e.to_user_facing())).await;
            },
        }
    });

    if app.app_state.is_generating() {
        // Already in mermaid's turn (agent loop continuation) — update handle, reset to Sending
        app.update_abort_handle(handle.abort_handle());
        app.transition_to_sending();
    } else {
        // New turn — enter generating state
        app.start_generation(handle.abort_handle());
    }
}

/// Run the main application event loop
///
/// This function coordinates all the specialized handlers and manages
/// the lifecycle of the TUI application.
///
/// The loop performs these steps each iteration:
/// 1. Render the UI
/// 2. Poll for events (keyboard, mouse)
/// 3. Process streaming chunks from LLM
/// 4. Handle events and delegate to specialized handlers
/// 5. Check for file system changes
/// 6. Auto-scroll management
pub async fn run_app_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    tx: mpsc::Sender<StreamEvent>,
    rx: &mut mpsc::Receiver<StreamEvent>,
) -> Result<()> {
    // Background title generation (non-blocking)
    let mut title_task: Option<tokio::task::JoinHandle<Option<String>>> = None;

    // Main event loop
    loop {
        // Poll for completed title generation (non-blocking)
        if let Some(ref task) = title_task
            && task.is_finished()
            && let Some(task) = title_task.take()
            && let Ok(Some(title)) = task.await
        {
            app.session_state.conversation_title = Some(title);
        }
        // Get viewport height for proper scrolling
        let viewport_height = terminal.size()?.height.saturating_sub(8); // 3 header + 3 input + 1 status + 1 margin

        // Draw UI
        terminal.draw(|f| render_ui(f, app))?;

        // Check if we should transition from Sending to Thinking (after 1 second with no chunks)
        if app.app_state.generation_status() == Some(GenerationStatus::Sending)
            && let Some(start_time) = app.app_state.generation_start_time()
            && start_time.elapsed().as_secs() >= 1
        {
            app.transition_to_thinking();
        }

        // Handle input events
        if event::poll(std::time::Duration::from_millis(UI_POLL_INTERVAL_MS))? {
            let event = event::read()?;

            // Use event_handler to process the event
            match handle_event(app, event, viewport_height)? {
                EventAction::Continue => {
                    // Continue normal loop
                },
                EventAction::Quit => {
                    break;
                },
                EventAction::SubmitMessage(input) => {
                    // Submit message to model
                    handle_message_submit(app, input, &tx, viewport_height).await;
                },
                EventAction::ExecuteCommand(command) => {
                    // Execute slash command
                    command_handler::handle_command(app, &command).await?;
                },
            }
        }

        // Process streaming responses
        match process_stream_chunks(app, rx).await? {
            StreamStatus::Streaming => {
                // During streaming: content is buffered and NOT rendered (block streaming mode)
                // Auto-scroll happens naturally via u16::MAX in render (if not user-scrolling)
            },
            StreamStatus::Complete { tool_calls } => {
                // Stream complete: response is now rendered

                // AGENT LOOP: Execute tool calls and continue until model stops
                if !tool_calls.is_empty() {
                    run_agent_loop(app, tool_calls, &tx, rx, terminal).await?;
                }

                // Process any queued messages after generation completes
                // This handles the case where user typed messages while model was generating
                // and there were no tool calls to trigger the agent loop
                'queue_loop: while app.operation_state.has_queued_message() {
                    if let Some(queued_msg) = app.operation_state.take_queued_message() {
                        // Submit the queued message as if user pressed Enter
                        handle_message_submit(app, queued_msg, &tx, viewport_height).await;

                        // Wait for this message's response to complete before sending next
                        loop {
                            // Check for interruption and handle scroll events
                            if render_and_check_interrupt(terminal, app)? {
                                // Clear remaining queued messages
                                let cleared = app.operation_state.queued_message_count();
                                while app.operation_state.take_queued_message().is_some() {}
                                if cleared > 0 {
                                    app.set_status(format!(
                                        "Interrupted - cleared {} queued message(s)",
                                        cleared
                                    ));
                                }
                                break 'queue_loop;
                            }

                            match process_stream_chunks(app, rx).await? {
                                StreamStatus::Streaming => {
                                    // Continue processing
                                },
                                StreamStatus::Complete {
                                    tool_calls: new_tool_calls,
                                } => {
                                    // If this response has tool calls, run agent loop
                                    if !new_tool_calls.is_empty() {
                                        run_agent_loop(app, new_tool_calls, &tx, rx, terminal)
                                            .await?;
                                    }
                                    break; // Done with this queued message
                                },
                                StreamStatus::Error(error) => {
                                    app.display_error(&error.summary, &error.message);
                                    app.stop_generation();
                                    break 'queue_loop;
                                },
                            }
                        }
                    }
                }

                // Mermaid's turn is over — return to user's turn
                app.stop_generation();

                // Spawn title generation in background (non-blocking)
                if title_task.is_none() {
                    title_task = app.spawn_title_generation();
                }
            },
            StreamStatus::Error(_error) => {
                // Error already handled by stream_handler (status message set)
                app.stop_generation();
            },
        }

        // Check if app should quit
        if !app.running {
            break;
        }
    }

    Ok(())
}

/// Handle message submission to the model
///
/// This spawns an async task to stream the model's response.
async fn handle_message_submit(
    app: &mut App,
    input: String,
    tx: &mpsc::Sender<StreamEvent>,
    _viewport_height: u16,
) {
    // Take any attached images before adding message
    let images = app.attachment_state.take_base64_data();

    // Store clean content — timestamp is in ChatMessage.timestamp field.
    // Temporal context is injected at API call time by build_managed_message_history.
    app.add_message_with_images(MessageRole::User, input.clone(), images);

    // Build message history including the new message
    let messages = app.build_managed_message_history(
        crate::constants::MAX_CONTEXT_TOKENS,
        crate::constants::CONTEXT_RESERVE_TOKENS,
    );

    // Auto-scroll happens naturally via u16::MAX in render (if not user-scrolling)

    // Save input to history and reset navigation
    app.input.history.push_back(input.clone());
    app.input.history_index = None;
    app.input.history_buffer.clear();

    // Persist to conversation if available
    if let Some(ref mut conv) = app.session_state.current_conversation {
        conv.add_to_input_history(input.clone());
        if let Some(ref manager) = app.session_state.conversation_manager {
            let conv_clone = conv.clone();
            let manager_clone = manager.clone();
            tokio::task::spawn_blocking(move || {
                let _ = manager_clone.save_conversation(&conv_clone);
            });
        }
    }

    // Process message asynchronously
    spawn_model_call(app, messages, tx);
}

/// Render the UI and check for interruption (non-blocking).
/// Handles all input events so the user can type, scroll, and queue messages
/// while the agent loop or model call is in progress.
/// Returns Ok(true) if the user wants to interrupt.
fn render_and_check_interrupt(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> Result<bool> {
    terminal.draw(|f| render_ui(f, app))?;

    while event::poll(Duration::from_millis(0))? {
        match event::read()? {
            event::Event::Key(key) if key.kind == crossterm::event::KeyEventKind::Press => {
                match key.code {
                    crossterm::event::KeyCode::Esc => {
                        let (abort, partial) = app.abort_generation();
                        if let Some(h) = abort {
                            h.abort();
                        }
                        if !partial.is_empty() {
                            app.add_message(MessageRole::Assistant, partial);
                        }
                        app.set_status("Agent loop interrupted");
                        return Ok(true);
                    },
                    crossterm::event::KeyCode::Char('c')
                        if key
                            .modifiers
                            .contains(crossterm::event::KeyModifiers::CONTROL) =>
                    {
                        let (abort, partial) = app.abort_generation();
                        if let Some(h) = abort {
                            h.abort();
                        }
                        if !partial.is_empty() {
                            app.add_message(MessageRole::Assistant, partial);
                        }
                        app.set_status("Agent loop interrupted");
                        return Ok(true);
                    },
                    // Typing: insert characters into input buffer
                    crossterm::event::KeyCode::Char(c) => {
                        if key.modifiers.is_empty()
                            || key.modifiers == crossterm::event::KeyModifiers::SHIFT
                        {
                            app.input.insert(c);
                        }
                    },
                    // Enter: queue the typed message for processing after current step
                    crossterm::event::KeyCode::Enter => {
                        if !app.input.is_empty() && !app.input.get().starts_with(':') {
                            let input = app.input.get().to_string();
                            app.operation_state.queue_message(input);
                            app.clear_input();
                            app.set_status("Message queued");
                        }
                    },
                    // Cursor movement and editing
                    crossterm::event::KeyCode::Backspace => {
                        app.input.backspace();
                    },
                    crossterm::event::KeyCode::Delete => {
                        app.input.delete();
                    },
                    crossterm::event::KeyCode::Left => {
                        app.input.move_left();
                    },
                    crossterm::event::KeyCode::Right => {
                        app.input.move_right();
                    },
                    crossterm::event::KeyCode::Home => {
                        app.input.move_home();
                    },
                    crossterm::event::KeyCode::End => {
                        app.input.move_end();
                    },
                    // Scrolling
                    crossterm::event::KeyCode::PageUp => {
                        app.scroll_up(10);
                    },
                    crossterm::event::KeyCode::PageDown => {
                        app.scroll_down(10);
                    },
                    _ => {},
                }
            },
            event::Event::Mouse(mouse) => match mouse.kind {
                crossterm::event::MouseEventKind::ScrollUp => {
                    app.scroll_up(crate::constants::UI_MOUSE_SCROLL_LINES);
                },
                crossterm::event::MouseEventKind::ScrollDown => {
                    app.scroll_down(crate::constants::UI_MOUSE_SCROLL_LINES);
                },
                _ => {},
            },
            // Handle paste events during agent loop
            event::Event::Paste(text) => {
                let cleaned = text.replace('\r', "");
                if !cleaned.is_empty() {
                    app.input.insert_str(&cleaned);
                }
            },
            _ => {},
        }
    }

    Ok(false)
}

/// TUI-specific agent loop for tool calling.
///
/// This is intentionally separate from `runtime::agent_loop::run_agent_loop`
/// because the TUI loop requires:
/// 1. Direct `&mut Terminal` access for `render_and_check_interrupt` — `Terminal`
///    is not `Send`, so it cannot be threaded through the `AgentObserver` trait.
/// 2. Live rendering of subagent progress via `poll_subagent_handles` with UI
///    updates between poll iterations.
/// 3. Queued message interception: the user can type and submit messages mid-loop,
///    which requires clearing tool calls, injecting a user message, and re-calling
///    the model — logic tightly coupled to TUI input state.
///
/// The shared loop in `runtime::agent_loop` is used by non-interactive mode
/// (via `SilentObserver`) and by sub-agents (via `SubagentObserver`).
///
/// This implements the proper agent loop pattern:
/// 1. Execute tool calls
/// 2. Add Tool messages for each result
/// 3. Call the model again
/// 4. Loop until no more tool_calls
///
/// Each completed step renders immediately to the TUI so the user
/// sees tool actions and model responses as they happen (block streaming).
///
/// This follows the Ollama API pattern documented at:
/// https://ollama.com/blog/tool-support
async fn run_agent_loop(
    app: &mut App,
    initial_tool_calls: Vec<crate::models::ToolCall>,
    tx: &mpsc::Sender<StreamEvent>,
    rx: &mut mpsc::Receiver<StreamEvent>,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
) -> Result<()> {
    let mut current_tool_calls = initial_tool_calls;
    let mut iteration = 0;

    while !current_tool_calls.is_empty() {
        iteration += 1;
        if iteration > MAX_AGENT_ITERATIONS {
            app.set_status(format!(
                "Agent loop exceeded {} iterations",
                MAX_AGENT_ITERATIONS
            ));
            return Ok(());
        }
        app.set_status(format!("Agent loop iteration {}", iteration));

        // Render so user sees iteration status; check for Esc interrupt
        if render_and_check_interrupt(terminal, app)? {
            return Ok(());
        }

        // Check for queued message BEFORE executing tool calls
        // This allows the user to intercept and redirect the agent
        if let Some(queued_msg) = app.operation_state.take_queued_message() {
            app.set_status("Processing queued message...");

            // Add the queued message as a user message (timestamp is in ChatMessage.timestamp)
            app.add_message(MessageRole::User, queued_msg.clone());

            // Save to input history
            app.input.history.push_back(queued_msg);

            // Clear current tool calls - the model will decide what to do next
            // based on the new user message
            current_tool_calls.clear();

            // Build message history and call model with the new context
            let messages = app.build_managed_message_history(
                crate::constants::MAX_CONTEXT_TOKENS,
                crate::constants::CONTEXT_RESERVE_TOKENS,
            );
            app.clear_response();

            spawn_model_call(app, messages, tx);

            // Wait for the model response (render each tick for live status)
            loop {
                if render_and_check_interrupt(terminal, app)? {
                    return Ok(());
                }

                match process_stream_chunks(app, rx).await? {
                    StreamStatus::Streaming => {},
                    StreamStatus::Complete {
                        tool_calls: new_tool_calls,
                    } => {
                        if !new_tool_calls.is_empty() {
                            current_tool_calls = new_tool_calls;
                        }
                        break;
                    },
                    StreamStatus::Error(error) => {
                        app.display_error(&error.summary, &error.message);
                        return Ok(());
                    },
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }

            // Continue the loop with potentially new tool calls
            continue;
        }

        // Partition tool calls: regular vs agent
        let (regular_calls, agent_calls): (Vec<&_>, Vec<&_>) = current_tool_calls
            .iter()
            .partition(|tc| tc.function.name != "agent");

        // Execute regular tool calls first (may write files that agents need to read)
        let regular_owned: Vec<_> = regular_calls.into_iter().cloned().collect();
        let mut results = if !regular_owned.is_empty() {
            action_handler::execute_tool_calls_for_agent_loop(app, &regular_owned).await
        } else {
            Vec::new()
        };

        // Execute agent tool calls in parallel
        if !agent_calls.is_empty() {
            let agent_specs: Vec<(String, String)> = agent_calls
                .iter()
                .filter_map(|tc| match tc.to_agent_action() {
                    Ok(AgentAction::SpawnAgent {
                        prompt,
                        description,
                    }) => Some((prompt, description)),
                    _ => None,
                })
                .collect();

            if !agent_specs.is_empty() {
                let progress = Arc::new(Mutex::new(Vec::<SubagentProgress>::new()));
                let config = app.build_model_config();

                // Store progress for UI rendering
                app.operation_state.active_subagents = Some(Arc::clone(&progress));

                let (handles, overflow) = spawn_subagents(
                    agent_specs,
                    Arc::clone(&app.model_state.model),
                    &config,
                    Arc::clone(&progress),
                );

                // Poll handles with render loop (TUI stays responsive)
                let subagent_results =
                    poll_subagent_handles(handles, overflow, terminal, app).await?;

                // Empty result means user interrupted (Esc) — exit cleanly
                // without stamping orphaned tool_calls onto the conversation
                if subagent_results.is_empty() {
                    return Ok(());
                }

                // Build ToolExecutionResult + ActionDisplay for each completed agent
                for (i, agent_result) in subagent_results.iter().enumerate() {
                    let action_display =
                        action_handler::build_agent_action_display(agent_result);

                    if let Some(last_msg) = app
                        .session_state
                        .messages
                        .iter_mut()
                        .rev()
                        .find(|m| matches!(m.role, MessageRole::Assistant))
                    {
                        last_msg.actions.push(action_display);
                    }

                    let tool_call_id = agent_calls
                        .get(i)
                        .and_then(|tc| tc.id.clone())
                        .unwrap_or_else(|| format!("call_agent_{}", i));

                    let output = format_subagent_tool_result(agent_result);

                    results.push(ToolExecutionResult {
                        tool_call_id,
                        tool_name: "agent".to_string(),
                        action: AgentAction::SpawnAgent {
                            prompt: String::new(),
                            description: agent_result.description.clone(),
                        },
                        success: agent_result.success,
                        output,
                        images: None,
                    });

                    app.session_state.cumulative_tokens += agent_result.tokens;
                }
            }
        }

        // Update the last assistant message to include tool_calls
        // (This is needed for the API to understand the conversation flow)
        if let Some(last_assistant) = app
            .session_state
            .messages
            .iter_mut()
            .rev()
            .find(|m| matches!(m.role, MessageRole::Assistant))
        {
            last_assistant.tool_calls = Some(current_tool_calls.clone());
        }

        // Add Tool messages for each result (with images for screenshot etc.)
        for result in &results {
            if let Some(ref imgs) = result.images {
                // Tool produced images (e.g., screenshot) — attach to tool message
                // so the model can see them on the next call
                let mut tool_msg = crate::models::ChatMessage::tool(
                    &result.tool_call_id,
                    &result.tool_name,
                    &result.output,
                );
                tool_msg = tool_msg.with_images(imgs.clone());
                app.commit_message(tool_msg);
            } else {
                app.add_tool_result(
                    result.tool_call_id.clone(),
                    result.tool_name.clone(),
                    result.output.clone(),
                );
            }
        }

        // Render to show completed tool actions immediately
        app.set_status(format!(
            "Iteration {} - {} tool(s) executed, calling model...",
            iteration,
            results.len()
        ));
        if render_and_check_interrupt(terminal, app)? {
            return Ok(());
        }

        // Note: Even if all tool calls failed, we continue the loop so the model
        // can see the error messages and retry with a different approach.

        // Call the model again with the updated message history
        let messages = app.build_managed_message_history(
            crate::constants::MAX_CONTEXT_TOKENS,
            crate::constants::CONTEXT_RESERVE_TOKENS,
        );
        app.clear_response();

        spawn_model_call(app, messages, tx);

        // Wait for the model response by processing stream chunks until Complete
        // Render on each tick so status bar updates and Esc works
        loop {
            // Render to show streaming progress (timer, tokens, status)
            if render_and_check_interrupt(terminal, app)? {
                return Ok(());
            }

            match process_stream_chunks(app, rx).await? {
                StreamStatus::Streaming => {
                    // Continue processing
                },
                StreamStatus::Complete {
                    tool_calls: new_tool_calls,
                } => {
                    // Got a new response - check if there are more tool calls
                    if new_tool_calls.is_empty() {
                        // No more tool calls - agent loop complete
                        app.set_status(format!(
                            "Agent loop complete after {} iterations",
                            iteration
                        ));
                        return Ok(());
                    } else {
                        // More tool calls - continue the loop
                        current_tool_calls = new_tool_calls;
                        break; // Break inner loop to continue outer agent loop
                    }
                },
                StreamStatus::Error(error) => {
                    // Error occurred - display and exit
                    app.display_error(&error.summary, &error.message);
                    return Ok(());
                },
            }

            // Sleep briefly to avoid busy-wait spin loop
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    Ok(())
}

/// Poll subagent JoinHandles with rendering between checks.
/// Aborts all handles and returns empty vec if user interrupts (Esc).
async fn poll_subagent_handles(
    handles: Vec<tokio::task::JoinHandle<SubagentResult>>,
    overflow: Vec<SubagentResult>,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> Result<Vec<SubagentResult>> {
    let handle_count = handles.len();
    app.set_status(format!("Running {} agent(s)...", handle_count));

    loop {
        // Update token count from subagent progress for the status line
        if let Some(ref progress) = app.operation_state.active_subagents {
            let total_tokens: usize = progress
                .lock()
                .map(|p| p.iter().map(|a| a.tokens).sum())
                .unwrap_or(0);
            if let super::state::AppState::Generating {
                tokens_received, ..
            } = &mut app.app_state
            {
                *tokens_received = total_tokens;
            }
        }

        // Render UI (shows live progress via active_subagents) and check for Esc
        if render_and_check_interrupt(terminal, app)? {
            for h in &handles {
                h.abort();
            }
            app.operation_state.clear_subagents();
            return Ok(Vec::new());
        }

        if handles.iter().all(|h| h.is_finished()) {
            break;
        }

        tokio::time::sleep(Duration::from_millis(UI_POLL_INTERVAL_MS)).await;
    }

    let results = collect_subagent_results(handles, overflow).await;

    app.set_status(format!("{} agent(s) completed", results.len()));
    app.operation_state.clear_subagents();

    Ok(results)
}
