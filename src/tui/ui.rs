use anyhow::Result;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers},
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
use crate::models::{ModelConfig, StreamCallback};
use crate::tui::{App, AppState};
use crate::tui::render::render_ui;

/// Run the terminal UI
pub async fn run_ui(mut app: App) -> Result<()> {
    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
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
    app_state: &mut AppState,
    tx: mpsc::Sender<String>,
    rx: &mut mpsc::Receiver<String>,
) -> Result<()> {
    loop {
        // Draw UI
        terminal.draw(|f| render_ui(f, app, app_state))?;

        // Handle streaming responses
        if app.is_generating {
            // Check for streamed content (non-blocking)
            if let Ok(chunk) = rx.try_recv() {
                app.current_response.push_str(&chunk);
                continue; // Immediately redraw with new content
            }
        }

        // Handle input events
        if event::poll(std::time::Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
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
                                            Ok(response) => {
                                                // Response is complete
                                                let _ = tx_done.send(format!("\n[DONE]:{}", response.content)).await;
                                            }
                                            Err(e) => {
                                                let _ = tx_done
                                                    .send(format!("\n[ERROR]:{}", e))
                                                    .await;
                                            }
                                        }
                                    });
                                }
                            }
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
                                app.input.pop();
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

                // Handle Ctrl+C to quit
                if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL {
                    app.quit();
                    break;
                }
            }
        }

        // Check for completion of generation
        if app.is_generating {
            if let Ok(chunk) = rx.try_recv() {
                if chunk.starts_with("[DONE]:") {
                    // Generation complete
                    app.is_generating = false;
                    let response = chunk.strip_prefix("[DONE]:").unwrap_or(&chunk);
                    app.add_message(
                        crate::tui::app::MessageRole::Assistant,
                        response.to_string(),
                    );
                    app.current_response.clear();

                    // Parse and execute any actions
                    let actions = agents::parse_actions(response);
                    for action in actions {
                        match agents::execute_action(&action).await {
                            Ok(agents::ActionResult::Success { output }) => {
                                app.set_status(format!("✓ Action completed: {}", output));
                            }
                            Ok(agents::ActionResult::Error { error }) => {
                                app.set_status(format!("✗ Action failed: {}", error));
                            }
                            Err(e) => {
                                app.set_status(format!("✗ Error: {}", e));
                            }
                        }
                    }
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
                    app.current_response.push_str(&chunk);
                }
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
                // TODO: Implement model switching
                app.set_status(format!("Switching to model: {}", model_name));
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