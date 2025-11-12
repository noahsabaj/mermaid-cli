use anyhow::Result;
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io;
use tokio::sync::mpsc;

use crate::tui::App;

/// Run the terminal UI
///
/// This function handles terminal setup, runs the main event loop via
/// loop_coordinator, and restores the terminal on exit.
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

    // Create channel for streaming responses
    let (tx, mut rx) = mpsc::channel::<String>(100);

    // Run the UI loop using the loop coordinator
    let res = super::loop_coordinator::run_app_loop(&mut terminal, &mut app, tx, &mut rx).await;

    // Save session state before exiting
    use crate::session::SessionState;
    let mut session = SessionState::load().unwrap_or_default();
    session.set_model(app.model_state.model_id.clone());
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
