//! Terminal setup and teardown.
//!
//! Raw mode, alternate screen, bracketed paste, mouse capture, and
//! panic-hook restoration — all entered and exited through
//! `TerminalGuard`.
//!
//! The `TerminalGuard` type is the important piece: putting teardown
//! inside a `Drop` impl means a panic in the render loop still
//! restores the user's shell, no matter where it happens.

use std::io::{self, Stdout};

use anyhow::{Context, Result};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

/// Owned terminal that restores the shell on drop.
///
/// Construct once at the top of `app::run`; keep it alive for the
/// duration of the main loop; let it drop. Do not construct twice
/// (the second `enable_raw_mode()` is idempotent but the second
/// `EnterAlternateScreen` stacks).
pub struct TerminalGuard {
    inner: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    pub fn setup() -> Result<Self> {
        enable_raw_mode().context("failed to enable raw mode")?;
        let mut stdout = io::stdout();
        execute!(
            stdout,
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste,
        )
        .context("failed to enter alternate screen / enable mouse / enable bracketed paste")?;

        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend).context("failed to create terminal")?;

        install_panic_hook();

        Ok(Self { inner: terminal })
    }

    /// Mutable access for the render pass.
    pub fn inner_mut(&mut self) -> &mut Terminal<CrosstermBackend<Stdout>> {
        &mut self.inner
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Best-effort restore — if any step fails we still try the
        // rest. The user will at least get raw mode off.
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            LeaveAlternateScreen,
            DisableMouseCapture,
            DisableBracketedPaste,
        );
        let _ = self.inner.show_cursor();
    }
}

/// Install a panic hook that restores the terminal before propagating
/// the panic. Without this, a panic mid-render leaves the user in raw
/// mode with the alternate screen still active — a shell unusable
/// until they type `reset` blind.
fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            LeaveAlternateScreen,
            DisableMouseCapture,
            DisableBracketedPaste,
        );
        original(info);
    }));
}
