use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers, MouseEventKind, EnableMouseCapture, DisableMouseCapture},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    Terminal,
};
use std::io;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::agents;
use crate::agents::ModeAwareExecutor;
use crate::models::{ModelConfig, StreamCallback};
use crate::tui::{App, AppState};
use crate::tui::render::render_ui;

/// Run the terminal UI
pub async fn run_ui(mut app: App) -> Result<()> {
    // Check if we have an interactive terminal
    if !crossterm::tty::IsTty::is_tty(&io::stdout()) {
        eprintln!("❌ Mermaid requires an interactive terminal.");
        eprintln!("   Cannot run in non-interactive mode (pipes, redirects, etc.)");
        eprintln!("   Try running directly in your terminal: mermaid");
        return Err(anyhow::anyhow!("No interactive terminal available"));
    }

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;  // Mouse capture disabled to allow text selection
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Clear terminal
    terminal.clear()?;

    let mut app_state = AppState::Insert;

    // Create channel for streaming responses
    let (tx, mut rx) = mpsc::channel::<String>(100);

    // Run the UI loop
    let res = run_app(&mut terminal, &mut app, &mut app_state, tx, &mut rx).await;

    // Restore terminal
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen
        // Mouse capture disabled to allow text selection
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
    app_state: &mut AppState,
    tx: mpsc::Sender<String>,
    rx: &mut mpsc::Receiver<String>,
) -> Result<()> {
    loop {
        // Draw UI
        terminal.draw(|f| render_ui(f, app, app_state))?;

        // Handle input events
        if event::poll(std::time::Duration::from_millis(50))? {
            match event::read()? {
                Event::Mouse(mouse) => {
                    // Handle mouse wheel scrolling in all states
                    match mouse.kind {
                        MouseEventKind::ScrollUp => {
                            app.scroll_down(3);  // Scroll up moves view down
                        }
                        MouseEventKind::ScrollDown => {
                            app.scroll_up(3);    // Scroll down moves view up
                        }
                        _ => {}
                    }
                }
                Event::Key(key) => {
                match app_state {
                    AppState::Normal => {
                        match key.code {
                            KeyCode::Char('q') => {
                                app.quit();
                                break;
                            }
                            KeyCode::Char('i') => {
                                *app_state = AppState::Insert;
                            }
                            KeyCode::Char(':') => {
                                *app_state = AppState::Command;
                                app.input.clear();
                            }
                            // Handle action confirmation
                            KeyCode::Char('y') | KeyCode::Char('Y') => {
                                if let (Some(action), Some(mut executor)) = (app.pending_action.take(), app.pending_executor.take()) {
                                    app.set_status("Executing action...");

                                    // Execute the confirmed action
                                    match executor.execute(action).await {
                                        Ok(agents::ActionResult::Success { output }) => {
                                            app.set_status(format!("✓ {}", output));
                                        }
                                        Ok(agents::ActionResult::Error { error }) => {
                                            app.set_status(format!("✗ Action failed: {}", error));
                                        }
                                        Err(e) => {
                                            app.set_status(format!("✗ Error: {}", e));
                                        }
                                    }
                                }
                            }
                            KeyCode::Char('n') | KeyCode::Char('N') => {
                                if app.pending_action.is_some() {
                                    app.pending_action = None;
                                    app.pending_executor = None;
                                    app.set_status("Action skipped");
                                }
                            }
                            KeyCode::Up => app.scroll_down(1),
                            KeyCode::Down => app.scroll_up(1),
                            KeyCode::PageUp => app.scroll_up(10),
                            KeyCode::PageDown => app.scroll_down(10),
                            KeyCode::Tab => app.toggle_sidebar(),
                            KeyCode::Char('e') => app.sidebar_expanded = !app.sidebar_expanded,
                            _ => {}
                        }
                    }
                    AppState::Insert => {
                        match key.code {
                            KeyCode::Esc => {
                                *app_state = AppState::Normal;
                            }
                            KeyCode::Enter => {
                                if !app.input.is_empty() && !app.is_generating {
                                    // Send message
                                    let input = app.input.clone();
                                    app.add_message(
                                        crate::tui::app::MessageRole::User,
                                        input.clone(),
                                    );
                                    app.clear_input();
                                    app.is_generating = true;
                                    app.current_response.clear();

                                    // Process message asynchronously
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
                                            .chat(&input, &context, &config, Some(callback))
                                            .await
                                        {
                                            Ok(_) => {
                                                // Response is complete - content already streamed via callback
                                                let _ = tx_done.send("[DONE]:".to_string()).await;
                                            }
                                            Err(e) => {
                                                let _ = tx_done
                                                    .send(format!("[ERROR]:{}", e))
                                                    .await;
                                            }
                                        }
                                    });
                                }
                            }
                            // Removed auto-switch to command mode - user should use Esc then ':'
                            KeyCode::Char(c) => {
                                app.input.push(c);
                            }
                            KeyCode::Backspace => {
                                app.input.pop();
                            }
                            _ => {}
                        }
                    }
                    AppState::Command => {
                        match key.code {
                            KeyCode::Esc => {
                                *app_state = AppState::Normal;
                                app.input.clear();
                            }
                            KeyCode::Enter => {
                                handle_command(app, &app.input.clone()).await?;
                                app.input.clear();
                                *app_state = AppState::Normal;
                            }
                            KeyCode::Char(c) => {
                                app.input.push(c);
                            }
                            KeyCode::Backspace => {
                                if app.input.is_empty() {
                                    // If input is empty, exit command mode
                                    *app_state = AppState::Insert;
                                } else {
                                    app.input.pop();
                                }
                            }
                            _ => {}
                        }
                    }
                    AppState::FileSelect => {
                        match key.code {
                            KeyCode::Esc => {
                                *app_state = AppState::Normal;
                            }
                            _ => {}
                        }
                    }
                }

                // Global keyboard shortcuts that work in any state

                // Handle Ctrl+C to quit
                if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL {
                    app.quit();
                    break;
                }

                // Handle Shift+Tab to cycle operation modes
                // Note: Some terminals report Shift+Tab as BackTab, others as Tab with SHIFT modifier
                if key.code == KeyCode::BackTab ||
                   (key.code == KeyCode::Tab && key.modifiers == KeyModifiers::SHIFT) {
                    app.cycle_mode();
                }

                // Handle Ctrl+Tab to cycle reverse (optional)
                if key.code == KeyCode::Tab && key.modifiers == KeyModifiers::CONTROL {
                    app.cycle_mode_reverse();
                }

                // Mode-specific shortcuts
                if key.modifiers == KeyModifiers::CONTROL {
                    match key.code {
                        KeyCode::Char('e') => app.set_mode(crate::tui::mode::OperationMode::AcceptEdits),
                        KeyCode::Char('p') => app.set_mode(crate::tui::mode::OperationMode::PlanMode),
                        KeyCode::Char('y') => app.toggle_bypass_mode(),
                        _ => {}
                    }
                }

                // Escape key returns to Normal mode from any operation mode
                if key.code == KeyCode::Esc && *app_state == AppState::Normal {
                    app.set_mode(crate::tui::mode::OperationMode::Normal);
                }
                }
                _ => {} // Ignore other events (FocusGained, FocusLost, Paste, Resize)
            }
        }

        // Handle streaming responses and check for completion
        if app.is_generating {
            // Process all available messages from the channel
            while let Ok(chunk) = rx.try_recv() {
                if chunk.starts_with("[DONE]:") {
                    // Generation complete
                    app.is_generating = false;
                    // Add the accumulated response from streaming
                    if !app.current_response.is_empty() {
                        let response_text = app.current_response.clone();
                        app.add_message(
                            crate::tui::app::MessageRole::Assistant,
                            response_text.clone(),
                        );

                        // Parse and execute any actions from the response
                        let actions = agents::parse_actions(&response_text);

                        // Create mode-aware executor
                        let mut executor = ModeAwareExecutor::new(app.operation_mode.clone());

                        for action in actions {
                            // Check if action needs confirmation
                            if executor.needs_confirmation(&action) {
                                // Show confirmation prompt
                                let action_desc = executor.describe_action(&action);
                                app.add_message(
                                    crate::tui::app::MessageRole::System,
                                    format!("🔔 Action requires confirmation:\n{}\n\nPress 'y' to confirm, 'n' to skip", action_desc),
                                );

                                // Store pending action for confirmation
                                app.pending_action = Some(action);
                                app.pending_executor = Some(executor);
                                break; // Wait for user confirmation
                            } else {
                                // Execute action directly
                                match executor.execute(action).await {
                                    Ok(agents::ActionResult::Success { output }) => {
                                        app.set_status(format!("✓ {}", output));
                                    }
                                    Ok(agents::ActionResult::Error { error }) => {
                                        app.set_status(format!("✗ Action failed: {}", error));
                                    }
                                    Err(e) => {
                                        app.set_status(format!("✗ Error: {}", e));
                                    }
                                }
                            }
                        }
                    }
                    app.current_response.clear();
                } else if chunk.starts_with("[ERROR]:") {
                    // Error occurred
                    app.is_generating = false;
                    let error = chunk.strip_prefix("[ERROR]:").unwrap_or(&chunk);
                    app.add_message(
                        crate::tui::app::MessageRole::System,
                        format!("Error: {}", error),
                    );
                    app.current_response.clear();
                } else {
                    // Regular chunk - append to current response
                    app.current_response.push_str(&chunk);
                }

                // Break after processing one message to allow redraw
                break;
            }
        }

        if !app.running {
            break;
        }
    }

    Ok(())
}

async fn handle_command(app: &mut App, command: &str) -> Result<()> {
    let parts: Vec<&str> = command.split_whitespace().collect();

    match parts.get(0).map(|s| *s) {
        Some("quit") | Some("q") => {
            app.quit();
        }
        Some("clear") => {
            app.messages.clear();
            app.set_status("Chat cleared");
        }
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
                use crate::models::ModelFactory;
                use crate::app::load_config;

                let config = match load_config() {
                    Ok(cfg) => cfg,
                    Err(e) => {
                        app.set_status(format!("Failed to load config: {}", e));
                        return Ok(());
                    }
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
                    }
                    Ok(Err(e)) => {
                        app.set_status(format!("Failed to switch model: {}", e));
                    }
                    Err(e) => {
                        app.set_status(format!("Failed to switch model: {}", e));
                    }
                }
            } else {
                app.set_status(format!("Current model: {}", app.model_name));
            }
        }
        Some("sidebar") | Some("sb") => {
            app.toggle_sidebar();
        }
        Some("help") | Some("h") => {
            app.add_message(
                crate::tui::app::MessageRole::System,
                "Commands:\n\
                 :quit/:q - Quit the application\n\
                 :clear - Clear chat history\n\
                 :model [name] - Switch model or show current\n\
                 :sidebar/:sb - Toggle file sidebar\n\
                 :help/:h - Show this help\n\
                 \n\
                 Keys:\n\
                 i - Enter insert mode (type messages)\n\
                 Esc - Return to normal mode\n\
                 : - Enter command mode\n\
                 Tab - Toggle sidebar\n\
                 Ctrl+C - Quit"
                    .to_string(),
            );
        }
        _ => {
            app.set_status(format!("Unknown command: {}", command));
        }
    }

    Ok(())
}