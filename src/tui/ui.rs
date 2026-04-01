use anyhow::Result;
use crossterm::{
    event::{DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};
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
        return Err(anyhow::anyhow!(
            "Mermaid requires an interactive terminal. Cannot run in non-interactive mode (pipes, redirects, etc.). Try running directly in your terminal: mermaid"
        ));
    }

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Clear terminal
    terminal.clear()?;

    // Create channel for streaming responses
    let (tx, mut rx) = mpsc::channel::<super::stream_event::StreamEvent>(1000);

    // Run the UI loop using the loop coordinator
    let res = super::loop_coordinator::run_app_loop(&mut terminal, &mut app, tx, &mut rx).await;

    // Shut down MCP servers (if any)
    if let Some(manager) = crate::agents::get_mcp_manager() {
        manager.shutdown().await;
    }

    // Restore terminal
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;

    res
}
