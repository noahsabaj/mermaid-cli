//! Terminal setup and teardown.
//!
//! Raw mode, alternate screen, bracketed paste, mouse capture, and
//! panic-hook restoration — all entered and exited through
//! `TerminalGuard`.
//!
//! The `TerminalGuard` type is the important piece: putting teardown
//! inside a `Drop` impl means a panic in the render loop still
//! restores the user's shell, no matter where it happens.

use std::io::{self, Stdout, Write};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use crossterm::cursor::Show;
use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    supports_keyboard_enhancement,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

static TERMINAL_NEEDS_RESTORE: AtomicBool = AtomicBool::new(false);

/// Whether the kitty keyboard-enhancement flags were pushed at setup — they
/// stack per screen buffer, so `restore_terminal` must pop them (before
/// leaving the alternate screen) exactly when they were pushed.
static KEYBOARD_ENHANCED: AtomicBool = AtomicBool::new(false);

/// Owned terminal that restores the shell on drop.
///
/// Construct once at the top of `app::run`; keep it alive for the
/// duration of the main loop; let it drop. Do not construct twice
/// (the second `enable_raw_mode()` is idempotent but the second
/// `EnterAlternateScreen` stacks).
pub struct TerminalGuard {
    inner: Terminal<CrosstermBackend<Stdout>>,
    restored: bool,
}

impl TerminalGuard {
    /// Take over the terminal: raw mode, alternate screen, mouse, bracketed
    /// paste, focus events.
    ///
    /// # Errors
    ///
    /// Enabling raw mode, entering the alternate screen and enabling the input
    /// modes, and building the backing `Terminal`. A failure after raw mode is
    /// on restores the terminal before returning, so an `Err` never leaves the
    /// shell in raw mode. A terminal without the kitty keyboard protocol is
    /// not an error — that probe degrades to the legacy encoding.
    pub fn setup() -> Result<Self> {
        enable_raw_mode().context("failed to enable raw mode")?;
        TERMINAL_NEEDS_RESTORE.store(true, Ordering::SeqCst);
        let mut stdout = io::stdout();
        if let Err(error) = execute!(
            stdout,
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste,
            EnableFocusChange,
        ) {
            restore_terminal_once();
            return Err(error).context(
                "failed to enter alternate screen / enable mouse / enable bracketed paste",
            );
        }

        // Kitty keyboard protocol, minimal tier: with only DISAMBIGUATE, keys
        // that are ambiguous in the legacy encoding arrive as distinct events
        // — Ctrl+Shift+C stops being byte-identical to Ctrl+C (so the copy
        // chord can't hit the quit path) and Esc stops being an Alt- prefix
        // guess. The probe needs raw mode (already on) and must run before
        // the main loop takes over the event reader. Terminals without the
        // protocol answer the probe negatively and keep legacy behavior.
        if matches!(supports_keyboard_enhancement(), Ok(true))
            && execute!(
                io::stdout(),
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
            )
            .is_ok()
        {
            KEYBOARD_ENHANCED.store(true, Ordering::SeqCst);
        }

        let backend = CrosstermBackend::new(stdout);
        let terminal = match Terminal::new(backend).context("failed to create terminal") {
            Ok(terminal) => terminal,
            Err(error) => {
                restore_terminal_once();
                return Err(error);
            },
        };

        install_panic_hook();

        Ok(Self {
            inner: terminal,
            restored: false,
        })
    }

    /// Mutable access for the render pass.
    pub fn inner_mut(&mut self) -> &mut Terminal<CrosstermBackend<Stdout>> {
        &mut self.inner
    }

    /// Restore terminal state now. Idempotent so normal exit, signal
    /// exit, and Drop can all share the same call site safely.
    pub fn restore_now(&mut self) {
        if self.restored {
            return;
        }
        restore_terminal_once();
        let _ = self.inner.show_cursor();
        self.restored = true;
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.restore_now();
    }
}

/// Best-effort terminal restore used by both normal Drop and panic
/// handling. The explicit escape fallback turns off every mouse mode
/// commonly emitted by terminals that support SGR mouse reporting;
/// this protects users when crossterm's higher-level cleanup does not
/// fully unwind a prior partial setup or a killed process left modes
/// dirty.
fn restore_terminal() {
    let mut stdout = io::stdout();
    // Pop the keyboard flags BEFORE leaving the alternate screen: kitty keeps
    // a separate flag stack per screen buffer, so popping after the switch
    // would pop the main screen's (empty) stack and leave the alt screen's
    // flags armed for the next full-screen app.
    if KEYBOARD_ENHANCED.swap(false, Ordering::SeqCst) {
        let _ = execute!(stdout, PopKeyboardEnhancementFlags);
    }
    let _ = execute!(
        stdout,
        DisableMouseCapture,
        DisableBracketedPaste,
        DisableFocusChange,
        LeaveAlternateScreen,
        Show,
    );
    // `\x1b[<u` (pop keyboard flags) rides in the raw fallback unconditionally:
    // terminals without the kitty protocol ignore the unknown CSI, same as the
    // mouse-mode resets below.
    let _ = stdout.write_all(
        b"\x1b[<u\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1004l\x1b[?1005l\x1b[?1006l\x1b[?1015l\x1b[?2004l\x1b[?1049l\x1b[?25h\x1b[0m",
    );
    let _ = stdout.flush();
    let _ = disable_raw_mode();
}

fn restore_terminal_once() {
    if !TERMINAL_NEEDS_RESTORE.swap(false, Ordering::SeqCst) {
        return;
    }
    restore_terminal();
}

/// Install a panic hook that restores the terminal before propagating
/// the panic. Without this, a panic mid-render leaves the user in raw
/// mode with the alternate screen still active — a shell unusable
/// until they type `reset` blind.
fn install_panic_hook() {
    static HOOK_INSTALLED: AtomicBool = AtomicBool::new(false);
    if HOOK_INSTALLED
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }

    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal_once();
        original(info);
    }));
}
