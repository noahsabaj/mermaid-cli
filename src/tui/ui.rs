use anyhow::Result;
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::agents;
use crate::agents::ModeAwareExecutor;
use crate::context::ContextLoader;
use crate::models::{MessageRole, ModelConfig, StreamCallback};
use crate::tui::render::render_ui;
use crate::tui::{App, ConfirmationState, FileInfo};
use crate::utils::{count_file_tokens, FileSystemWatcher};

/// Run the terminal UI
pub async fn run_ui(mut app: App) -> Result<()> {
    // Check if we have an interactive terminal
    if !crossterm::tty::IsTty::is_tty(&io::stdout()) {
        eprintln!("[ERROR] Mermaid requires an interactive terminal.");
        eprintln!("   Cannot run in non-interactive mode (pipes, redirects, etc.)");
        eprintln!("   Try running directly in your terminal: mermaid");
        return Err(anyhow::anyhow!("No interactive terminal available"));
    }

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Clear terminal
    terminal.clear()?;

    // No more app_state - we're always in chat mode

    // Create channel for streaming responses
    let (tx, mut rx) = mpsc::channel::<String>(100);

    // Run the UI loop
    let res = run_app(&mut terminal, &mut app, tx, &mut rx).await;

    // Save session state before exiting
    use crate::session::SessionState;
    let mut session = SessionState::load().unwrap_or_default();
    session.set_model(app.model_name.clone());
    if let Err(e) = session.save() {
        eprintln!("[WARNING] Failed to save session: {}", e);
    }

    // Restore terminal
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    if let Err(err) = res {
        eprintln!("Error: {:?}", err);
    }

    Ok(())
}

async fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    tx: mpsc::Sender<String>,
    rx: &mut mpsc::Receiver<String>,
) -> Result<()> {
    // Initialize file watcher for the current directory
    let watcher = FileSystemWatcher::new(Path::new("."))?;
    let mut last_refresh = std::time::Instant::now();

    // Start hardware monitoring if available
    let hardware_monitor = app.hardware_monitor.clone();
    let hardware_tx = tx.clone();
    if let Some(monitor) = hardware_monitor {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(2));
            loop {
                interval.tick().await;
                let stats = {
                    let mut m = monitor.lock().await;
                    m.get_stats()
                };
                if let Ok(stats) = stats {
                    // Send hardware stats as JSON
                    if let Ok(json) = serde_json::to_string(&stats) {
                        let _ = hardware_tx.send(format!("[HARDWARE_STATS]:{}", json)).await;
                    }
                }
            }
        });
    }
    loop {
        // Get viewport height for proper scrolling
        let viewport_height = terminal.size()?.height.saturating_sub(8); // 3 header + 3 input + 1 status + 1 margin

        // Draw UI
        terminal.draw(|f| render_ui(f, app))?;

        // Handle input events
        if event::poll(std::time::Duration::from_millis(50))? {
            match event::read()? {
                Event::Mouse(mouse) => {
                    // Handle mouse wheel scrolling in all states
                    match mouse.kind {
                        MouseEventKind::ScrollUp => {
                            app.scroll_down(3); // Scroll up moves view down
                            app.is_user_scrolling = true; // User is scrolling
                        },
                        MouseEventKind::ScrollDown => {
                            app.scroll_up(3); // Scroll down moves view up
                                              // Check if scrolled to bottom
                            let max_scroll = app.calculate_max_scroll(viewport_height);
                            if app.scroll_offset >= max_scroll.saturating_sub(3) {
                                app.is_user_scrolling = false;
                                app.scroll_offset = max_scroll;
                            }
                        },
                        _ => {},
                    }
                },
                Event::Key(key) => {
                    // Handle Alt+key combinations globally for confirmation
                    if key.modifiers == KeyModifiers::ALT && app.confirmation_state.is_some() {
                        match key.code {
                            KeyCode::Char('y') | KeyCode::Char('Y') => {
                                // Approve action
                                if let Some(confirmation) = app.confirmation_state.take() {
                                    app.set_status(format!(
                                        "Executing: {}...",
                                        confirmation.action_description
                                    ));

                                    // Get the executor from pending_executor
                                    if let Some(mut executor) = app.pending_executor.take() {
                                        let action_clone = confirmation.action.clone();

                                        // Execute the action
                                        match executor.execute(confirmation.action).await {
                                            Ok(agents::ActionResult::Success { output }) => {
                                                handle_action_success(
                                                    app,
                                                    &action_clone,
                                                    output,
                                                    &tx,
                                                )
                                                .await;
                                            },
                                            Ok(agents::ActionResult::Error { error }) => {
                                                app.set_status(format!(
                                                    "[FAILED] Action failed: {}",
                                                    error
                                                ));
                                            },
                                            Err(e) => {
                                                app.set_status(format!("[ERROR] Error: {}", e));
                                            },
                                        }
                                    }
                                    app.pending_action = None;
                                }
                            },
                            KeyCode::Char('n') | KeyCode::Char('N') => {
                                // Skip action
                                if let Some(_) = app.confirmation_state.take() {
                                    app.set_status("Action skipped");
                                    app.pending_action = None;
                                    app.pending_executor = None;
                                }
                            },
                            KeyCode::Char('a') | KeyCode::Char('A') => {
                                // Always approve - persistent preferences not yet implemented
                                if let Some(confirmation) = app.confirmation_state.take() {
                                    app.set_status("Always approving similar actions");
                                    // For now, just approve this one
                                    if let Some(mut executor) = app.pending_executor.take() {
                                        let action_clone = confirmation.action.clone();
                                        match executor.execute(confirmation.action).await {
                                            Ok(agents::ActionResult::Success { output }) => {
                                                handle_action_success(
                                                    app,
                                                    &action_clone,
                                                    output,
                                                    &tx,
                                                )
                                                .await;
                                            },
                                            Ok(agents::ActionResult::Error { error }) => {
                                                app.set_status(format!(
                                                    "[FAILED] Action failed: {}",
                                                    error
                                                ));
                                            },
                                            Err(e) => {
                                                app.set_status(format!("[ERROR] Error: {}", e));
                                            },
                                        }
                                    }
                                    app.pending_action = None;
                                }
                            },
                            KeyCode::Char('p') | KeyCode::Char('P') => {
                                // Toggle preview - full preview expansion not yet implemented
                                app.set_status("Preview toggled");
                            },
                            _ => {},
                        }
                        continue; // Skip normal key handling when confirmation is active
                    }

                    // Simplified key handling - no modes
                    match key.code {
                        KeyCode::Esc => {
                            use crate::diagnostics::DiagnosticsMode;

                            // If diagnostics panel is open, close it
                            if app.diagnostics_mode == DiagnosticsMode::Detailed {
                                app.diagnostics_mode = DiagnosticsMode::Compact;
                                app.set_status("Diagnostics panel closed");
                            } else if app.is_generating {
                                // If generating, abort the generation but keep what was generated
                                if let Some(abort) = app.generation_abort.take() {
                                    abort.abort();
                                }
                                app.is_generating = false;

                                // Save partial response instead of clearing it
                                if !app.current_response.is_empty() {
                                    app.add_message(
                                        MessageRole::Assistant,
                                        app.current_response.clone(),
                                    );
                                    app.current_response.clear();
                                }
                                app.set_status("Generation stopped");
                            } else if !app.input.is_empty() {
                                // Clear input if not generating
                                app.input.clear();
                                app.cursor_position = 0; // Reset cursor when clearing
                                app.set_status("Input cleared");
                            }
                        },
                        KeyCode::Enter => {
                            if !app.input.is_empty() && !app.is_generating {
                                // Check if this is a command (starts with ':')
                                if app.input.starts_with(':') {
                                    // Execute command
                                    let command = app.input.trim_start_matches(':').to_string();
                                    handle_command(app, &command).await?;
                                    app.clear_input();
                                } else {
                                    // Clear any stuck status messages when sending new message
                                    app.pending_file_read = false;
                                    app.reading_file_status = None;

                                    // Send message
                                    let input = app.input.clone();
                                    app.add_message(MessageRole::User, input.clone());
                                    app.clear_input();

                                    // Build message history including the new message
                                    let messages = app.build_message_history();

                                    // Auto-scroll to show the new user message
                                    app.auto_scroll_to_bottom(viewport_height);
                                    app.is_generating = true;
                                    app.current_response.clear();

                                    // Process message asynchronously
                                    let model = app.model.clone();
                                    let context = app.context.clone();
                                    let tx_clone = tx.clone();
                                    let tx_done = tx.clone();

                                    let handle = tokio::spawn(async move {
                                        let config = ModelConfig::default();
                                        let callback: StreamCallback = Arc::new(move |chunk| {
                                            let _ = tx_clone.try_send(chunk.to_string());
                                        });

                                        let mut model = model.lock().await;
                                        match model
                                            .chat(&messages, &context, &config, Some(callback))
                                            .await
                                        {
                                            Ok(_) => {
                                                // Response is complete - content already streamed via callback
                                                let _ = tx_done.send("[DONE]:".to_string()).await;
                                            },
                                            Err(e) => {
                                                let _ =
                                                    tx_done.send(format!("[ERROR]:{}", e)).await;
                                            },
                                        }
                                    });
                                    app.generation_abort = Some(handle.abort_handle());
                                }
                            }
                        },
                        KeyCode::Char(c) => {
                            // Insert character at cursor position
                            app.input.insert(app.cursor_position, c);
                            app.cursor_position += 1;
                        },
                        KeyCode::Backspace => {
                            if app.cursor_position > 0 {
                                app.cursor_position -= 1;
                                app.input.remove(app.cursor_position);
                            }
                        },
                        KeyCode::Delete => {
                            if app.cursor_position < app.input.len() {
                                app.input.remove(app.cursor_position);
                            }
                        },
                        KeyCode::Left => {
                            if app.cursor_position > 0 {
                                app.cursor_position -= 1;
                            }
                        },
                        KeyCode::Right => {
                            if app.cursor_position < app.input.len() {
                                app.cursor_position += 1;
                            }
                        },
                        KeyCode::Home => {
                            app.cursor_position = 0;
                        },
                        KeyCode::End => {
                            app.cursor_position = app.input.len();
                        },
                        // Navigation keys always available
                        KeyCode::Up => app.scroll_down(1),
                        KeyCode::Down => app.scroll_up(1),
                        KeyCode::PageUp => app.scroll_up(10),
                        KeyCode::PageDown => app.scroll_down(10),
                        KeyCode::Tab => app.toggle_sidebar(),
                        _ => {},
                    }

                    // Global keyboard shortcuts that work in any state

                    // Handle Ctrl+C to quit
                    if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL {
                        app.auto_save_conversation();
                        app.quit();
                        break;
                    }

                    // Handle Shift+Tab to cycle operation modes
                    // Note: Some terminals report Shift+Tab as BackTab, others as Tab with SHIFT modifier
                    if key.code == KeyCode::BackTab
                        || (key.code == KeyCode::Tab && key.modifiers == KeyModifiers::SHIFT)
                    {
                        app.cycle_mode();
                    }

                    // Handle Ctrl+Tab to cycle reverse (optional)
                    if key.code == KeyCode::Tab && key.modifiers == KeyModifiers::CONTROL {
                        app.cycle_mode_reverse();
                    }

                    // F2 to toggle diagnostics
                    if key.code == KeyCode::F(2) {
                        app.toggle_diagnostics();
                    }

                    // Mode-specific shortcuts
                    if key.modifiers == KeyModifiers::CONTROL {
                        match key.code {
                            KeyCode::Char('e') => {
                                app.set_mode(crate::tui::mode::OperationMode::AcceptEdits)
                            },
                            KeyCode::Char('p') => {
                                app.set_mode(crate::tui::mode::OperationMode::PlanMode)
                            },
                            KeyCode::Char('y') => app.toggle_bypass_mode(),
                            _ => {},
                        }
                    }
                },
                _ => {}, // Ignore other events (FocusGained, FocusLost, Paste, Resize)
            }
        } // Close the if event::poll(...) block

        // Handle streaming responses and check for completion
        if app.is_generating {
            // Process all available messages from the channel
            while let Ok(chunk) = rx.try_recv() {
                if chunk.starts_with("[DONE]:") {
                    // Check if this is feedback completion
                    let is_feedback_complete = chunk.contains("[FEEDBACK_COMPLETE]");

                    // Generation complete
                    app.is_generating = false;

                    // Clear feedback flags if this was a feedback response
                    if is_feedback_complete {
                        app.pending_file_read = false;
                        app.reading_file_status = None;
                    }

                    // Also clear any lingering file read status on normal completion
                    // This prevents the status from getting stuck
                    if !app.pending_file_read {
                        app.reading_file_status = None;
                    }

                    // Add the accumulated response from streaming
                    if !app.current_response.is_empty() {
                        let response_text = app.current_response.clone();
                        app.add_message(MessageRole::Assistant, response_text.clone());

                        // Parse and execute any actions from the response
                        let actions = agents::parse_actions(&response_text);

                        // Check if any actions will trigger feedback loops
                        let has_feedback_actions = actions
                            .iter()
                            .any(|a| matches!(a, agents::AgentAction::ReadFile { .. }));

                        if has_feedback_actions {
                            app.pending_file_read = true;
                            // Extract the model's intent from text before [FILE_READ]
                            app.reading_file_status = extract_reading_intent(&response_text);
                            if app.reading_file_status.is_some() {
                                app.status_timestamp = Some(std::time::Instant::now());
                            }
                        }

                        // Create mode-aware executor
                        let mut executor = ModeAwareExecutor::new(app.operation_mode.clone());

                        for action in actions {
                            // Check if action needs confirmation
                            if executor.needs_confirmation(&action) {
                                // Create confirmation state for inline display
                                let action_desc = executor.describe_action(&action);

                                // Extract preview and file info for WriteFile actions
                                let (preview_lines, file_info) = match &action {
                                    agents::AgentAction::WriteFile { path, content } => {
                                        let lines: Vec<String> = content
                                            .lines()
                                            .take(5)
                                            .map(|s| s.to_string())
                                            .collect();
                                        let info = FileInfo {
                                            path: path.clone(),
                                            size: content.len(),
                                            exists: Path::new(path).exists(),
                                            language: detect_language(path),
                                        };
                                        (lines, Some(info))
                                    },
                                    _ => (vec![], None),
                                };

                                // Set confirmation state
                                app.confirmation_state = Some(ConfirmationState {
                                    action: action.clone(),
                                    action_description: action_desc,
                                    preview_lines,
                                    file_info,
                                    allow_always: matches!(
                                        action,
                                        agents::AgentAction::WriteFile { .. }
                                    ),
                                });

                                // Store executor for later use
                                app.pending_action = Some(action);
                                app.pending_executor = Some(executor);
                                break; // Wait for user confirmation
                            } else {
                                // Clone action to check type after execution
                                let action_clone = action.clone();

                                // Execute action directly
                                match executor.execute(action).await {
                                    Ok(agents::ActionResult::Success { output }) => {
                                        // Handle ReadFile specially - show contents in chat
                                        match &action_clone {
                                            agents::AgentAction::ReadFile { path } => {
                                                // Feedback loop: Send file contents back to model
                                                app.set_status(format!("[OK] File read: {}", path));
                                                // Keep pending_file_read true during feedback
                                                app.is_generating = true;
                                                app.current_response.clear();

                                                // Create a prompt for the model to present the file contents
                                                let feedback_prompt = format!(
                                                    "I've successfully read the file '{}'. Here are its contents:\n\n{}\n\nPlease present these contents to the user in a helpful way.",
                                                    path, output
                                                );

                                                // Add feedback as system message and build history
                                                app.add_message(
                                                    MessageRole::System,
                                                    feedback_prompt.clone(),
                                                );
                                                let messages = app.build_message_history();

                                                // Send feedback to model
                                                let model = app.model.clone();
                                                let context = app.context.clone();
                                                let tx_clone = tx.clone();
                                                let tx_done = tx.clone();

                                                tokio::spawn(async move {
                                                    let config = ModelConfig::default();
                                                    let callback: StreamCallback =
                                                        Arc::new(move |chunk| {
                                                            let _ = tx_clone
                                                                .try_send(chunk.to_string());
                                                        });

                                                    let mut model = model.lock().await;
                                                    match model
                                                        .chat(
                                                            &messages,
                                                            &context,
                                                            &config,
                                                            Some(callback),
                                                        )
                                                        .await
                                                    {
                                                        Ok(_) => {
                                                            // Clear feedback flags after completion
                                                            let _ = tx_done
                                                                .send(
                                                                    "[DONE]:[FEEDBACK_COMPLETE]"
                                                                        .to_string(),
                                                                )
                                                                .await;
                                                        },
                                                        Err(e) => {
                                                            let _ = tx_done
                                                                .send(format!("[ERROR]:{}", e))
                                                                .await;
                                                        },
                                                    }
                                                });
                                            },
                                            agents::AgentAction::WriteFile { path, content } => {
                                                app.set_status(format!("[OK] {}", output));
                                                app.context.add_file(path.clone(), content.clone());
                                                // Use proper tokenizer for accurate count
                                                let tokens =
                                                    count_file_tokens(content, &app.model_name);
                                                app.context.token_count += tokens;
                                            },
                                            agents::AgentAction::DeleteFile { path } => {
                                                app.set_status(format!("[OK] {}", output));
                                                if let Some(content) =
                                                    app.context.files.remove(path)
                                                {
                                                    // Use proper tokenizer for accurate count
                                                    let tokens = count_file_tokens(
                                                        &content,
                                                        &app.model_name,
                                                    );
                                                    app.context.token_count = app
                                                        .context
                                                        .token_count
                                                        .saturating_sub(tokens);
                                                }
                                            },
                                            _ => {
                                                app.set_status(format!("[OK] {}", output));
                                            },
                                        }
                                    },
                                    Ok(agents::ActionResult::Error { error }) => {
                                        app.set_status(format!(
                                            "[FAILED] Action failed: {}",
                                            error
                                        ));
                                    },
                                    Err(e) => {
                                        app.set_status(format!("[ERROR] Error: {}", e));
                                    },
                                }
                            }
                        }
                    }
                    app.current_response.clear();
                } else if chunk.starts_with("[ERROR]:") {
                    // Error occurred
                    app.is_generating = false;
                    let error = chunk.strip_prefix("[ERROR]:").unwrap_or(&chunk);
                    app.add_message(MessageRole::System, format!("Error: {}", error));
                    app.current_response.clear();
                } else if chunk.starts_with("[HARDWARE_STATS]:") {
                    // Hardware stats update
                    if let Some(json_str) = chunk.strip_prefix("[HARDWARE_STATS]:") {
                        if let Ok(stats) =
                            serde_json::from_str::<crate::diagnostics::HardwareStats>(json_str)
                        {
                            app.hardware_stats = Some(stats);
                        }
                    }
                } else {
                    // Regular chunk - append to current response
                    app.current_response.push_str(&chunk);

                    // Auto-scroll to bottom during generation if user isn't manually scrolling
                    app.auto_scroll_to_bottom(viewport_height);
                }

                // Break after processing one message to allow redraw
                break;
            }
        }

        // Always check for hardware stats updates (even when not generating)
        while let Ok(chunk) = rx.try_recv() {
            if chunk.starts_with("[HARDWARE_STATS]:") {
                // Hardware stats update
                if let Some(json_str) = chunk.strip_prefix("[HARDWARE_STATS]:") {
                    if let Ok(stats) =
                        serde_json::from_str::<crate::diagnostics::HardwareStats>(json_str)
                    {
                        app.hardware_stats = Some(stats);
                    }
                }
                break; // Process one update per loop iteration
            } else if !app.is_generating {
                // If we're not generating and it's not a hardware stats message,
                // put it back for later processing when generation starts
                // Note: This is a simplified approach - in production you might want a queue
                break;
            }
        }

        // Check for external file system changes (throttled to once per second)
        if last_refresh.elapsed() >= std::time::Duration::from_secs(1) {
            let events = watcher.check_events();
            if !events.is_empty() {
                // Reload the context to pick up external changes
                if let Ok(loader) = ContextLoader::new() {
                    if let Ok(new_context) = loader.load(Path::new(".")) {
                        // Update the context while preserving conversation history
                        app.context.files = new_context.files;
                        app.context.token_count = new_context.token_count;
                        app.set_status("[OK] Files refreshed from disk");
                    }
                }
                last_refresh = std::time::Instant::now();
            }
        }

        // Clear stale file reading status after 5 seconds
        if app.reading_file_status.is_some() && !app.is_generating {
            if let Some(timestamp) = app.status_timestamp {
                if timestamp.elapsed() >= std::time::Duration::from_secs(5) {
                    app.reading_file_status = None;
                    app.pending_file_read = false;
                    app.status_timestamp = None;
                }
            }
        }

        if !app.running {
            break;
        }
    }

    Ok(())
}

/// Extract the model's reading intent from the text before [FILE_READ]
fn extract_reading_intent(text: &str) -> Option<String> {
    // Find the FILE_READ action block
    if let Some(idx) = text.find("[FILE_READ:") {
        // Get the text before the action
        let before = &text[..idx];

        // Find the file path from the action block
        let file_path = if let Some(end_idx) = text[idx..].find(']') {
            let path_part = &text[idx + 11..idx + end_idx]; // Skip "[FILE_READ:"
            path_part.trim()
        } else {
            "the file"
        };

        // Generate contextual status based on the model's preceding text
        let status = if before.contains("read") || before.contains("Read") {
            format!("Reading {}...", file_path)
        } else if before.contains("check") || before.contains("Check") {
            format!("Checking {}...", file_path)
        } else if before.contains("look") || before.contains("Look") {
            format!("Looking at {}...", file_path)
        } else if before.contains("open") || before.contains("Open") {
            format!("Opening {}...", file_path)
        } else if before.contains("examine") || before.contains("Examine") {
            format!("Examining {}...", file_path)
        } else if before.contains("load") || before.contains("Load") {
            format!("Loading {}...", file_path)
        } else {
            format!("Processing {}...", file_path)
        };

        Some(status)
    } else {
        None
    }
}

async fn handle_command(app: &mut App, command: &str) -> Result<()> {
    let parts: Vec<&str> = command.split_whitespace().collect();

    match parts.get(0).map(|s| *s) {
        Some("quit") | Some("q") => {
            app.auto_save_conversation();
            app.quit();
        },
        Some("clear") => {
            app.messages.clear();
            app.set_status("Chat cleared");
        },
        Some("model") => {
            if let Some(model_name) = parts.get(1) {
                // Parse the model name (could be provider/model or just model)
                let model_id = if model_name.contains('/') {
                    model_name.to_string()
                } else {
                    // Assume ollama if no provider specified
                    format!("ollama/{}", model_name)
                };

                app.set_status(format!("Switching to model: {}...", model_id));

                // Try to create the new model
                use crate::app::load_config;
                use crate::models::ModelFactory;

                let config = match load_config() {
                    Ok(cfg) => cfg,
                    Err(e) => {
                        app.set_status(format!("Failed to load config: {}", e));
                        return Ok(());
                    },
                };

                // Create new model asynchronously
                let model_id_clone = model_id.clone();
                let new_model = tokio::task::spawn(async move {
                    ModelFactory::create(&model_id_clone, Some(&config)).await
                });

                match new_model.await {
                    Ok(Ok(model)) => {
                        // Update the model and model name
                        *app.model.lock().await = model;
                        app.model_name = model_id.clone();
                        app.set_status(format!("Switched to model: {}", model_id));

                        // Save the model preference to session
                        use crate::session::SessionState;
                        let mut session = SessionState::load().unwrap_or_default();
                        session.set_model(model_id);
                        let _ = session.save();
                    },
                    Ok(Err(e)) => {
                        app.set_status(format!("Failed to switch model: {}", e));
                    },
                    Err(e) => {
                        app.set_status(format!("Failed to switch model: {}", e));
                    },
                }
            } else {
                app.set_status(format!("Current model: {}", app.model_name));
            }
        },
        Some("sidebar") | Some("sb") => {
            app.toggle_sidebar();
        },
        Some("refresh") | Some("r") => {
            // Manually refresh file context from disk
            match ContextLoader::new() {
                Ok(loader) => match loader.load(Path::new(".")) {
                    Ok(new_context) => {
                        app.context.files = new_context.files;
                        app.context.token_count = new_context.token_count;
                        app.set_status(format!(
                            "[OK] Refreshed: {} files, ~{} tokens",
                            app.context.files.len(),
                            app.context.token_count
                        ));
                    },
                    Err(e) => {
                        app.set_status(format!("[FAILED] Failed to refresh: {}", e));
                    },
                },
                Err(e) => {
                    app.set_status(format!("[FAILED] Failed to create loader: {}", e));
                },
            }
        },
        Some("save") => {
            // Save conversation with optional name
            let name = parts.get(1).map(|s| s.to_string());
            if let Err(e) = app.save_conversation() {
                app.set_status(format!("Failed to save: {}", e));
            } else {
                app.set_status(if name.is_some() {
                    format!("Conversation saved as: {}", name.unwrap())
                } else {
                    "Conversation saved".to_string()
                });
            }
        },
        Some("load") => {
            // Load a conversation by name or show selector
            if let Some(ref manager) = app.conversation_manager {
                if let Some(name) = parts.get(1) {
                    // Load specific conversation
                    match manager.load_conversation(name) {
                        Ok(conv) => {
                            app.load_conversation(conv);
                        },
                        Err(e) => {
                            app.set_status(format!("Failed to load: {}", e));
                        },
                    }
                } else {
                    // Show list of available conversations
                    match manager.list_conversations() {
                        Ok(conversations) => {
                            if conversations.is_empty() {
                                app.set_status("No saved conversations found");
                            } else {
                                let list = conversations
                                    .iter()
                                    .map(|c| c.summary())
                                    .collect::<Vec<_>>()
                                    .join("\n");
                                app.add_message(
                                    MessageRole::System,
                                    format!("Available conversations:\n{}\n\nUse :load <id> to load a specific conversation", list),
                                );
                            }
                        },
                        Err(e) => {
                            app.set_status(format!("Failed to list conversations: {}", e));
                        },
                    }
                }
            }
        },
        Some("stats") | Some("diag") | Some("diagnostics") => {
            // Toggle diagnostics display
            app.toggle_diagnostics();
        },
        Some("list") => {
            // List saved conversations
            if let Some(ref manager) = app.conversation_manager {
                match manager.list_conversations() {
                    Ok(conversations) => {
                        if conversations.is_empty() {
                            app.set_status("No saved conversations in this directory");
                        } else {
                            let list = conversations
                                .iter()
                                .map(|c| c.summary())
                                .collect::<Vec<_>>()
                                .join("\n");
                            app.add_message(
                                MessageRole::System,
                                format!("Saved conversations:\n{}", list),
                            );
                        }
                    },
                    Err(e) => {
                        app.set_status(format!("Failed to list conversations: {}", e));
                    },
                }
            }
        },
        Some("help") | Some("h") => {
            app.add_message(
                MessageRole::System,
                "Commands:\n\
                 :quit/:q - Quit the application\n\
                 :clear - Clear chat history\n\
                 :model [name] - Switch model or show current\n\
                 :sidebar/:sb - Toggle file sidebar\n\
                 :refresh/:r - Refresh file context from disk\n\
                 :save [name] - Save current conversation\n\
                 :load [name] - Load a conversation\n\
                 :list - List saved conversations\n\
                 :stats/:diag - Toggle hardware diagnostics\n\
                 :help/:h - Show this help\n\
                 \n\
                 Keys:\n\
                 i - Enter insert mode (type messages)\n\
                 Esc - Return to normal mode / Close diagnostics\n\
                 : - Enter command mode\n\
                 Tab - Toggle sidebar\n\
                 F2 - Toggle hardware diagnostics\n\
                 Ctrl+C - Quit"
                    .to_string(),
            );
        },
        _ => {
            app.set_status(format!("Unknown command: {}", command));
        },
    }

    Ok(())
}

/// Handle successful action execution
async fn handle_action_success(
    app: &mut App,
    action: &agents::AgentAction,
    output: String,
    tx: &mpsc::Sender<String>,
) {
    match action {
        agents::AgentAction::ReadFile { path } => {
            // Feedback loop: Send file contents back to model
            app.set_status(format!("[OK] File read: {}", path));

            // Set feedback tracking
            app.pending_file_read = true;
            if app.reading_file_status.is_none() {
                app.reading_file_status = Some(format!("Processing {}...", path));
                app.status_timestamp = Some(std::time::Instant::now());
            }

            app.is_generating = true;
            app.current_response.clear();

            // Create a prompt for the model to present the file contents
            let feedback_prompt = format!(
                "I've successfully read the file '{}'. Here are its contents:\n\n{}\n\nPlease present these contents to the user in a helpful way.",
                path, output
            );

            // Add feedback as system message and build history
            app.add_message(MessageRole::System, feedback_prompt.clone());
            let messages = app.build_message_history();

            // Send feedback to model
            let model = app.model.clone();
            let context = app.context.clone();
            let tx_clone = tx.clone();
            let tx_done = tx.clone();

            tokio::spawn(async move {
                let config = ModelConfig::default();
                let callback: StreamCallback = Arc::new(move |chunk| {
                    let _ = tx_clone.try_send(chunk.to_string());
                });

                let mut model = model.lock().await;
                match model
                    .chat(&messages, &context, &config, Some(callback))
                    .await
                {
                    Ok(_) => {
                        // Clear feedback flags after completion
                        let _ = tx_done.send("[DONE]:[FEEDBACK_COMPLETE]".to_string()).await;
                    },
                    Err(e) => {
                        let _ = tx_done.send(format!("[ERROR]:{}", e)).await;
                    },
                }
            });
        },
        agents::AgentAction::WriteFile { path, content } => {
            app.set_status(format!("[OK] {}", output));
            app.context.add_file(path.clone(), content.clone());
            // Use proper tokenizer for accurate count
            let tokens = count_file_tokens(content, &app.model_name);
            app.context.token_count += tokens;
        },
        agents::AgentAction::DeleteFile { path } => {
            app.set_status(format!("[OK] {}", output));
            if let Some(content) = app.context.files.remove(path) {
                // Use proper tokenizer for accurate count
                let tokens = count_file_tokens(&content, &app.model_name);
                app.context.token_count = app.context.token_count.saturating_sub(tokens);
            }
        },
        _ => {
            app.set_status(format!("[OK] {}", output));
        },
    }
}

/// Detect language from file extension
fn detect_language(path: &str) -> Option<String> {
    let ext = Path::new(path).extension().and_then(|e| e.to_str())?;

    match ext {
        "rs" => Some("Rust".to_string()),
        "py" => Some("Python".to_string()),
        "js" | "jsx" => Some("JavaScript".to_string()),
        "ts" | "tsx" => Some("TypeScript".to_string()),
        "go" => Some("Go".to_string()),
        "java" => Some("Java".to_string()),
        "c" => Some("C".to_string()),
        "cpp" | "cc" | "cxx" => Some("C++".to_string()),
        "h" | "hpp" => Some("C/C++ Header".to_string()),
        "rb" => Some("Ruby".to_string()),
        "php" => Some("PHP".to_string()),
        "swift" => Some("Swift".to_string()),
        "kt" => Some("Kotlin".to_string()),
        "scala" => Some("Scala".to_string()),
        "sh" | "bash" => Some("Shell".to_string()),
        "yaml" | "yml" => Some("YAML".to_string()),
        "toml" => Some("TOML".to_string()),
        "json" => Some("JSON".to_string()),
        "xml" => Some("XML".to_string()),
        "html" => Some("HTML".to_string()),
        "css" | "scss" | "sass" => Some("CSS".to_string()),
        "md" => Some("Markdown".to_string()),
        _ => None,
    }
}
