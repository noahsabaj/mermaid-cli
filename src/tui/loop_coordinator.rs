use anyhow::Result;
use chrono::Local;
use crossterm::event;
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::context::Context;
use crate::models::{MessageRole, ModelConfig, StreamCallback};
use crate::searxng::ensure_searxng_running;
use super::state::GenerationStatus;
use crate::tui::render::render_ui;
use crate::tui::App;
use crate::utils::FileSystemWatcher;

/// Import our specialized handlers
use super::action_handler;
use super::command_handler;
use super::event_handler::{handle_event, EventAction};
use super::stream_handler::{process_stream_chunks, StreamStatus};

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
    tx: mpsc::Sender<String>,
    rx: &mut mpsc::Receiver<String>,
) -> Result<()> {
    // Initialize file watcher for the current directory
    let watcher = FileSystemWatcher::new(Path::new("."))?;
    let mut last_refresh = std::time::Instant::now();

    // Start Searxng in the background for web search capability
    // This runs silently without blocking the UI
    tokio::spawn(async {
        ensure_searxng_running().await;
    });

    // Main event loop
    loop {
        // Get viewport height for proper scrolling
        let viewport_height = terminal.size()?.height.saturating_sub(8); // 3 header + 3 input + 1 status + 1 margin

        // Draw UI
        terminal.draw(|f| render_ui(f, app))?;

        // Check if we should transition from Sending to Thinking (after 1 second with no chunks)
        if app.app_state.generation_status() == Some(GenerationStatus::Sending) {
            if let Some(start_time) = app.app_state.generation_start_time() {
                if start_time.elapsed().as_secs() >= 1 {
                    app.transition_to_thinking();
                }
            }
        }

        // Handle input events
        if event::poll(std::time::Duration::from_millis(50))? {
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
                EventAction::ConfirmAction(approved) => {
                    // Confirm or reject pending action
                    action_handler::confirm_action(app, approved, &tx).await?;
                },
                EventAction::AlwaysApprove => {
                    // Always approve similar actions (not yet fully implemented)
                    action_handler::confirm_action(app, true, &tx).await?;
                },
                EventAction::TogglePreview => {
                    // Toggle action preview (not yet fully implemented)
                    app.set_status("Preview toggled");
                },
                EventAction::ApprovePlan => {
                    // Approve and start executing plan
                    action_handler::approve_plan(app, &tx).await?;
                },
                EventAction::CancelPlan => {
                    // Cancel pending plan
                    action_handler::cancel_plan(app);
                },
            }
        }

        // Process streaming responses
        match process_stream_chunks(app, rx).await? {
            StreamStatus::Streaming => {
                // During streaming: content is buffered and NOT rendered (block streaming mode)
                // Auto-scroll happens naturally via u16::MAX in render (if not user-scrolling)
            },
            StreamStatus::Complete { actions, tool_calls } => {
                // Stream complete: response is now rendered
                // Auto-scroll happens naturally via u16::MAX in render (if not user-scrolling)

                // AGENT LOOP: Execute tool calls and continue until no more tool_calls
                if !tool_calls.is_empty() {
                    run_agent_loop(app, tool_calls, &tx, rx).await?;
                } else if !actions.is_empty() {
                    // Legacy path: actions without tool_calls (backwards compatibility)
                    action_handler::execute_actions(app, actions, &tx).await?;
                }

                // Generate conversation title after first exchange (if not already generated)
                if app.session_state.conversation_title.is_none() && app.session_state.messages.len() >= 2 {
                    tokio::spawn(async move {
                        // This runs in background to avoid blocking the UI
                        // Title will be picked up in next render cycle
                    });
                    // Actually generate title inline for simplicity
                    app.generate_conversation_title().await;
                }
            },
            StreamStatus::PlanReady { plan } => {
                // Plan mode: store plan and wait for user approval
                // Note: Message already added in stream_handler with full explanation + plan
                app.set_plan(plan.clone());
            },
            StreamStatus::FeedbackComplete => {
                // Feedback loop complete, nothing to do
            },
            StreamStatus::Error(_error) => {
                // Error already handled by stream_handler (status message set)
            },
        }

        // Check for external file system changes (throttled to once per second)
        if last_refresh.elapsed() >= std::time::Duration::from_secs(1) {
            let events = watcher.check_events();
            if !events.is_empty() {
                // Reload the context to pick up external changes
                if let Ok(new_context) = Context::load(Path::new(".")).await {
                    // Update the context while preserving conversation history
                    app.context.files = new_context.files;
                    app.context.token_count = new_context.token_count;
                    app.set_status("[OK] Files refreshed from disk");
                }
                last_refresh = std::time::Instant::now();
            }
        }

        // Clear stale file reading status after 5 seconds
        if app.operation_state.reading_file_status.is_some() && !app.app_state.is_generating() {
            if let Some(timestamp) = app.status_state.status_timestamp {
                if timestamp.elapsed() >= std::time::Duration::from_secs(5) {
                    app.operation_state.reading_file_status = None;
                    app.operation_state.pending_file_read = false;
                    app.status_state.status_timestamp = None;
                }
            }
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
    tx: &mpsc::Sender<String>,
    _viewport_height: u16,
) {
    // Clear any stuck status messages when sending new message
    app.operation_state.pending_file_read = false;
    app.operation_state.reading_file_status = None;

    // Add timestamp to message for temporal awareness
    let timestamp = Local::now().format("%Y-%m-%d %H:%M:%S %Z").to_string();
    let timestamped_input = format!("[Sent at: {}]\n{}", timestamp, input);

    // Add user message to history with timestamp
    app.add_message(MessageRole::User, timestamped_input);

    // Build message history including the new message
    let messages = app.build_message_history();

    // Auto-scroll happens naturally via u16::MAX in render (if not user-scrolling)
    app.current_response.clear();

    // Save input to history and reset navigation
    app.session_state.input_history.push(input.clone());
    app.session_state.history_index = None;
    app.session_state.history_buffer.clear();

    // Persist to conversation if available
    if let Some(ref mut conv) = app.session_state.current_conversation {
        conv.add_to_input_history(input.clone());
        if let Some(ref manager) = app.session_state.conversation_manager {
            let _ = manager.save_conversation(conv);
        }
    }

    // Process message asynchronously
    let model = app.model_state.model.clone();
    let context = app.context.clone();
    let tx_clone = tx.clone();
    let tx_done = tx.clone();

    let operation_mode = app.operation_state.operation_mode.clone();
    let model_id = app.model_state.model_id.clone();

    let handle = tokio::spawn(async move {
        // Use Plan Mode config if in Plan Mode, otherwise use default
        let mut config = if operation_mode.is_planning_only() {
            ModelConfig::with_plan_mode()
        } else {
            ModelConfig::default()
        };

        config.model = model_id.clone();

        let callback: StreamCallback = Arc::new(move |chunk| {
            let _ = tx_clone.try_send(chunk.to_string());
        });

        let model = model.write().await;
        match model
            .chat(&messages, &context, &config, Some(callback))
            .await
        {
            Ok(response) => {
                // Send real token count from Ollama with [DONE] message
                let tokens = response.usage.map(|u| u.completion_tokens).unwrap_or(0);
                let _ = tx_done.send(format!("[DONE]:tokens={}", tokens)).await;
            },
            Err(e) => {
                // Send structured error for rich UX display
                let error_json = e.to_channel_message();
                let _ = tx_done.send(format!("[ERROR_JSON]:{}", error_json)).await;
            },
        }
    });

    // Start generation state with abort handle
    app.start_generation(handle.abort_handle());
}

/// Run the agent loop for tool calling
///
/// This implements the proper agent loop pattern:
/// 1. Execute tool calls
/// 2. Add Tool messages for each result
/// 3. Call the model again
/// 4. Loop until no more tool_calls
///
/// This follows the Ollama API pattern documented at:
/// https://ollama.com/blog/tool-support
async fn run_agent_loop(
    app: &mut App,
    initial_tool_calls: Vec<crate::models::ToolCall>,
    tx: &mpsc::Sender<String>,
    rx: &mut mpsc::Receiver<String>,
) -> Result<()> {
    let mut current_tool_calls = initial_tool_calls;
    let mut iteration = 0;

    while !current_tool_calls.is_empty() {
        iteration += 1;
        app.set_status(format!("Agent loop iteration {}", iteration));

        // Execute tool calls and get results
        let results = action_handler::execute_tool_calls_for_agent_loop(app, &current_tool_calls).await;

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

        // Add Tool messages for each result
        for result in &results {
            app.add_tool_result(
                result.tool_call_id.clone(),
                result.tool_name.clone(),
                result.content.clone(),
            );
        }

        // Check if any results failed in a way that should stop the loop
        let all_failed = results.iter().all(|r| !r.success);
        if all_failed && !results.is_empty() {
            app.set_status("Agent loop stopped: all tool calls failed");
            break;
        }

        // Call the model again with the updated message history
        let messages = app.build_message_history();
        app.current_response.clear();

        let model = app.model_state.model.clone();
        let context = app.context.clone();
        let tx_clone = tx.clone();
        let tx_done = tx.clone();
        let model_id = app.model_state.model_id.clone();
        let operation_mode = app.operation_state.operation_mode.clone();

        let handle = tokio::spawn(async move {
            let mut config = if operation_mode.is_planning_only() {
                ModelConfig::with_plan_mode()
            } else {
                ModelConfig::default()
            };
            config.model = model_id;

            let callback: StreamCallback = Arc::new(move |chunk| {
                let _ = tx_clone.try_send(chunk.to_string());
            });

            let model = model.write().await;
            match model
                .chat(&messages, &context, &config, Some(callback))
                .await
            {
                Ok(response) => {
                    // Send real token count from Ollama with [DONE] message
                    let tokens = response.usage.map(|u| u.completion_tokens).unwrap_or(0);
                    let _ = tx_done.send(format!("[DONE]:tokens={}", tokens)).await;
                },
                Err(e) => {
                    let error_json = e.to_channel_message();
                    let _ = tx_done.send(format!("[ERROR_JSON]:{}", error_json)).await;
                },
            }
        });

        app.start_generation(handle.abort_handle());

        // Wait for the model response by processing stream chunks until Complete
        loop {
            match process_stream_chunks(app, rx).await? {
                StreamStatus::Streaming => {
                    // Continue processing
                },
                StreamStatus::Complete { actions: _, tool_calls: new_tool_calls } => {
                    // Got a new response - check if there are more tool calls
                    if new_tool_calls.is_empty() {
                        // No more tool calls - agent loop complete
                        app.set_status(format!("Agent loop complete after {} iterations", iteration));
                        return Ok(());
                    } else {
                        // More tool calls - continue the loop
                        current_tool_calls = new_tool_calls;
                        break; // Break inner loop to continue outer agent loop
                    }
                },
                StreamStatus::PlanReady { plan } => {
                    // Plan mode response - store and exit
                    app.set_plan(plan);
                    return Ok(());
                },
                StreamStatus::FeedbackComplete => {
                    // Feedback complete - exit loop
                    return Ok(());
                },
                StreamStatus::Error(error) => {
                    // Error occurred - display and exit
                    app.display_error(&error.summary, &error.message);
                    return Ok(());
                },
            }

            // Yield to allow UI updates
            tokio::task::yield_now().await;
        }
    }

    Ok(())
}
