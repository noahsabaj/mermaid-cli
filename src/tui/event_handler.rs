use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyModifiers, MouseEventKind};

use crate::tui::App;

/// Actions that can result from handling an event
#[derive(Debug, Clone)]
pub enum EventAction {
    /// Continue normal event loop
    Continue,
    /// Quit the application
    Quit,
    /// Submit a message to the model
    SubmitMessage(String),
    /// Execute a slash command
    ExecuteCommand(String),
    /// Confirm or reject a pending action (true = approve, false = reject)
    ConfirmAction(bool),
    /// Always approve similar actions (persistent preference)
    AlwaysApprove,
    /// Toggle action preview
    TogglePreview,
    /// Approve and start executing a pending plan
    ApprovePlan,
    /// Cancel/reject a pending plan
    CancelPlan,
}

/// Handle a single event and return the appropriate action
///
/// This function is pure event routing - it does NOT execute business logic.
/// It just determines what action should be taken based on the event.
pub fn handle_event(app: &mut App, event: Event, viewport_height: u16) -> Result<EventAction> {
    match event {
        Event::Mouse(mouse) => handle_mouse_event(app, mouse, viewport_height),
        Event::Key(key) => handle_key_event(app, key, viewport_height),
        _ => Ok(EventAction::Continue), // Ignore FocusGained, FocusLost, Paste, Resize
    }
}

/// Handle keyboard events for plan approval/cancellation during plan mode
fn handle_plan_approval_keys(key_code: KeyCode) -> Result<EventAction> {
    match key_code {
        KeyCode::Char('y') | KeyCode::Char('Y') => Ok(EventAction::ApprovePlan),
        KeyCode::Char('n') | KeyCode::Char('N') => Ok(EventAction::CancelPlan),
        _ => Ok(EventAction::Continue),
    }
}

/// Handle mouse events (primarily scrolling)
fn handle_mouse_event(
    app: &mut App,
    mouse: crossterm::event::MouseEvent,
    viewport_height: u16,
) -> Result<EventAction> {
    match mouse.kind {
        MouseEventKind::ScrollUp => {
            app.scroll_down(3); // Scroll up moves view down
            app.chat_state.is_user_scrolling = true;
            Ok(EventAction::Continue)
        },
        MouseEventKind::ScrollDown => {
            app.scroll_up(3); // Scroll down moves view up

            // Check if scrolled to bottom
            let max_scroll = app.calculate_max_scroll(viewport_height);
            if app.chat_state.scroll_offset >= max_scroll.saturating_sub(3) {
                app.chat_state.is_user_scrolling = false;
                app.chat_state.scroll_offset = max_scroll;
            }
            Ok(EventAction::Continue)
        },
        _ => Ok(EventAction::Continue),
    }
}

/// Handle keyboard events
fn handle_key_event(
    app: &mut App,
    key: crossterm::event::KeyEvent,
    _viewport_height: u16,
) -> Result<EventAction> {
    // Handle Alt+key combinations globally for plan approval/cancellation
    if key.modifiers == KeyModifiers::ALT && app.awaiting_plan_approval {
        return handle_plan_approval_keys(key.code);
    }

    // Handle Alt+key combinations for confirmation
    if key.modifiers == KeyModifiers::ALT && app.confirmation_state.is_some() {
        return handle_confirmation_keys(key.code);
    }

    // Handle normal keyboard shortcuts
    let action = match key.code {
        KeyCode::Esc => handle_escape_key(app),
        KeyCode::Enter => handle_enter_key(app)?,
        KeyCode::Char(c) => handle_char_input(app, c, key.modifiers),
        KeyCode::Backspace => handle_backspace(app),
        KeyCode::Delete => handle_delete(app),
        KeyCode::Left => handle_left_arrow(app),
        KeyCode::Right => handle_right_arrow(app),
        KeyCode::Home => handle_home(app),
        KeyCode::End => handle_end(app),
        KeyCode::Up => handle_up_arrow(app),
        KeyCode::Down => handle_down_arrow(app),
        KeyCode::PageUp => handle_page_up(app),
        KeyCode::PageDown => handle_page_down(app),
        KeyCode::Tab => handle_tab(app, key.modifiers),
        KeyCode::BackTab => handle_backtab(app),
        KeyCode::F(2) => handle_f2(app),
        _ => EventAction::Continue,
    };

    Ok(action)
}

/// Handle Alt+key combinations during confirmation
fn handle_confirmation_keys(key_code: KeyCode) -> Result<EventAction> {
    match key_code {
        KeyCode::Char('y') | KeyCode::Char('Y') => Ok(EventAction::ConfirmAction(true)),
        KeyCode::Char('n') | KeyCode::Char('N') => Ok(EventAction::ConfirmAction(false)),
        KeyCode::Char('a') | KeyCode::Char('A') => Ok(EventAction::AlwaysApprove),
        KeyCode::Char('p') | KeyCode::Char('P') => Ok(EventAction::TogglePreview),
        _ => Ok(EventAction::Continue),
    }
}

/// Handle Escape key (close diagnostics, stop generation, cancel plan, or clear input)
fn handle_escape_key(app: &mut App) -> EventAction {
    use crate::diagnostics::DiagnosticsMode;

    // If diagnostics panel is open, close it
    if app.diagnostics_mode == DiagnosticsMode::Detailed {
        app.diagnostics_mode = DiagnosticsMode::Compact;
        app.set_status("Diagnostics panel closed");
    } else if app.awaiting_plan_approval {
        // Cancel pending plan
        app.cancel_plan();
        return EventAction::Continue;
    } else if app.is_generating {
        // If generating, abort the generation but keep what was generated
        if let Some(abort) = app.generation_abort.take() {
            abort.abort();
        }
        app.is_generating = false;

        // Save partial response instead of clearing it
        if !app.current_response.is_empty() {
            use crate::models::MessageRole;
            app.add_message(MessageRole::Assistant, app.current_response.clone());
            app.current_response.clear();
        }
        app.set_status("Generation stopped");
    } else if !app.input.is_empty() {
        // Clear input if not generating
        app.input.clear();
        app.cursor_position = 0;
        app.set_status("Input cleared");
    }

    EventAction::Continue
}

/// Handle Enter key (submit message or command)
fn handle_enter_key(app: &mut App) -> Result<EventAction> {
    // If waiting for plan approval, block new messages
    if app.awaiting_plan_approval {
        app.set_status("Complete or cancel the plan first (Alt+Y to approve, Alt+N to cancel)");
        return Ok(EventAction::Continue);
    }

    if !app.input.is_empty() && !app.is_generating {
        // Check if this is a command (starts with ':')
        if app.input.starts_with(':') {
            let command = app.input.trim_start_matches(':').to_string();
            app.clear_input();
            Ok(EventAction::ExecuteCommand(command))
        } else {
            let input = app.input.clone();
            app.clear_input();
            Ok(EventAction::SubmitMessage(input))
        }
    } else {
        Ok(EventAction::Continue)
    }
}

/// Handle character input (with modifier check for shortcuts)
fn handle_char_input(app: &mut App, c: char, modifiers: KeyModifiers) -> EventAction {
    // Ctrl+C to quit
    if c == 'c' && modifiers == KeyModifiers::CONTROL {
        app.auto_save_conversation();
        app.quit();
        return EventAction::Quit;
    }

    // Mode-specific shortcuts
    if modifiers == KeyModifiers::CONTROL {
        match c {
            'd' => {
                app.toggle_diagnostics();
                return EventAction::Continue;
            },
            'e' => {
                app.set_mode(crate::tui::mode::OperationMode::AcceptEdits);
                return EventAction::Continue;
            },
            'p' => {
                app.set_mode(crate::tui::mode::OperationMode::PlanMode);
                return EventAction::Continue;
            },
            's' => {
                app.toggle_sidebar();
                return EventAction::Continue;
            },
            'y' => {
                app.toggle_bypass_mode();
                return EventAction::Continue;
            },
            _ => {},
        }
    }

    // Normal character input (no modifiers or only SHIFT for uppercase)
    if modifiers.is_empty() || modifiers == KeyModifiers::SHIFT {
        app.input.insert(app.cursor_position, c);
        app.cursor_position += 1;
    }

    EventAction::Continue
}

/// Handle Backspace key
fn handle_backspace(app: &mut App) -> EventAction {
    if app.cursor_position > 0 {
        app.cursor_position -= 1;
        app.input.remove(app.cursor_position);
    }
    EventAction::Continue
}

/// Handle Delete key
fn handle_delete(app: &mut App) -> EventAction {
    if app.cursor_position < app.input.len() {
        app.input.remove(app.cursor_position);
    }
    EventAction::Continue
}

/// Handle Left arrow key
fn handle_left_arrow(app: &mut App) -> EventAction {
    if app.cursor_position > 0 {
        app.cursor_position -= 1;
    }
    EventAction::Continue
}

/// Handle Right arrow key
fn handle_right_arrow(app: &mut App) -> EventAction {
    if app.cursor_position < app.input.len() {
        app.cursor_position += 1;
    }
    EventAction::Continue
}

/// Handle Home key
fn handle_home(app: &mut App) -> EventAction {
    app.cursor_position = 0;
    EventAction::Continue
}

/// Handle End key
fn handle_end(app: &mut App) -> EventAction {
    app.cursor_position = app.input.len();
    EventAction::Continue
}

/// Handle Up arrow (scroll down in chat)
fn handle_up_arrow(app: &mut App) -> EventAction {
    app.scroll_down(1);
    EventAction::Continue
}

/// Handle Down arrow (scroll up in chat)
fn handle_down_arrow(app: &mut App) -> EventAction {
    app.scroll_up(1);
    EventAction::Continue
}

/// Handle Page Up (scroll up in chat)
fn handle_page_up(app: &mut App) -> EventAction {
    app.scroll_up(10);
    EventAction::Continue
}

/// Handle Page Down (scroll down in chat)
fn handle_page_down(app: &mut App) -> EventAction {
    app.scroll_down(10);
    EventAction::Continue
}

/// Handle Tab key (with modifier check for mode cycling)
fn handle_tab(app: &mut App, modifiers: KeyModifiers) -> EventAction {
    if modifiers == KeyModifiers::SHIFT {
        // Shift+Tab cycles operation modes
        app.cycle_mode();
    } else if modifiers == KeyModifiers::CONTROL {
        // Ctrl+Tab cycles reverse
        app.cycle_mode_reverse();
    } else {
        // Plain Tab toggles sidebar
        app.toggle_sidebar();
    }
    EventAction::Continue
}

/// Handle BackTab (Shift+Tab on some terminals)
fn handle_backtab(app: &mut App) -> EventAction {
    app.cycle_mode();
    EventAction::Continue
}

/// Handle F2 key (toggle diagnostics)
fn handle_f2(app: &mut App) -> EventAction {
    app.toggle_diagnostics();
    EventAction::Continue
}
