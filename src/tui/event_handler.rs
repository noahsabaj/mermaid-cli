use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};

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
}

/// Handle a single event and return the appropriate action
///
/// This function is pure event routing - it does NOT execute business logic.
/// It just determines what action should be taken based on the event.
pub fn handle_event(app: &mut App, event: Event, viewport_height: u16) -> Result<EventAction> {
    match event {
        Event::Mouse(mouse) => handle_mouse_event(app, mouse, viewport_height),
        Event::Key(key) => handle_key_event(app, key, viewport_height),
        Event::Paste(text) => handle_paste(app, &text),
        _ => Ok(EventAction::Continue), // Ignore FocusGained, FocusLost, Resize
    }
}

/// Handle mouse events (primarily scrolling)
fn handle_mouse_event(
    app: &mut App,
    mouse: crossterm::event::MouseEvent,
    _viewport_height: u16,
) -> Result<EventAction> {
    match mouse.kind {
        MouseEventKind::ScrollUp => {
            app.scroll_up(3); // Wheel up shows older messages (viewport moves up)
            Ok(EventAction::Continue)
        },
        MouseEventKind::ScrollDown => {
            app.scroll_down(3); // Wheel down shows newer messages (viewport moves down)
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
    // Only handle key press events, not release or repeat.
    // On Windows, crossterm sends both Press and Release events for each keystroke,
    // which would cause duplicate character input without this filter.
    if key.kind != KeyEventKind::Press {
        return Ok(EventAction::Continue);
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
        _ => EventAction::Continue,
    };

    Ok(action)
}

/// Handle Escape key (stop generation or clear input)
fn handle_escape_key(app: &mut App) -> EventAction {
    if app.app_state.is_generating() {
        // If generating, abort the generation but keep what was generated
        if let Some(abort) = app.abort_generation() {
            abort.abort();
        }

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
        app.set_status("Input cleared");
    }

    EventAction::Continue
}

/// Handle Enter key (submit message, queue message, or command)
fn handle_enter_key(app: &mut App) -> Result<EventAction> {
    if app.input.is_empty() {
        return Ok(EventAction::Continue);
    }

    // Check if this is a command (starts with ':')
    if app.input.get().starts_with(':') {
        // Commands only work when not generating
        if !app.app_state.is_generating() {
            let command = app.input.get().trim_start_matches(':').to_string();
            app.clear_input();
            return Ok(EventAction::ExecuteCommand(command));
        }
        return Ok(EventAction::Continue);
    }

    // If generating, queue the message instead of submitting
    if app.app_state.is_generating() {
        let input = app.input.get().to_string();
        app.operation_state.queue_message(input);
        app.clear_input();
        app.set_status("Message queued - will be sent before next action");
        return Ok(EventAction::Continue);
    }

    // Normal submission when not generating
    let input = app.input.get().to_string();
    app.clear_input();
    Ok(EventAction::SubmitMessage(input))
}

/// Handle pasted text (bracketed paste mode)
/// Newlines in pasted text are replaced with spaces so multi-line pastes
/// become a single input rather than triggering multiple submissions.
fn handle_paste(app: &mut App, text: &str) -> Result<EventAction> {
    // Replace newlines with spaces so pasted text stays as one input
    let cleaned = text.replace('\n', " ").replace('\r', "");
    // Collapse multiple spaces from blank lines
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    if !collapsed.is_empty() {
        // Reset history navigation when pasting
        if app.session_state.history_index.is_some() {
            app.session_state.history_index = None;
            app.session_state.history_buffer.clear();
        }
        app.input.insert_str(&collapsed);
    }
    Ok(EventAction::Continue)
}

/// Handle character input (with modifier check for shortcuts)
fn handle_char_input(app: &mut App, c: char, modifiers: KeyModifiers) -> EventAction {
    // Ctrl+C to quit
    if c == 'c' && modifiers == KeyModifiers::CONTROL {
        app.auto_save_conversation();
        app.quit();
        return EventAction::Quit;
    }

    // Alt+T to toggle thinking mode
    if c == 't' && modifiers == KeyModifiers::ALT {
        match app.model_state.toggle_thinking() {
            Some(true) => app.set_status("Thinking mode enabled"),
            Some(false) => app.set_status("Thinking mode disabled"),
            None => app.set_status("Model does not support thinking"),
        }
        return EventAction::Continue;
    }

    // Normal character input (no modifiers or only SHIFT for uppercase)
    if modifiers.is_empty() || modifiers == KeyModifiers::SHIFT {
        // Reset history navigation when user starts typing
        if app.session_state.history_index.is_some() {
            app.session_state.history_index = None;
            app.session_state.history_buffer.clear();
        }
        app.input.insert(c);
    }

    EventAction::Continue
}

/// Handle Backspace key
fn handle_backspace(app: &mut App) -> EventAction {
    app.input.backspace();
    EventAction::Continue
}

/// Handle Delete key
fn handle_delete(app: &mut App) -> EventAction {
    app.input.delete();
    EventAction::Continue
}

/// Handle Left arrow key
fn handle_left_arrow(app: &mut App) -> EventAction {
    app.input.move_left();
    EventAction::Continue
}

/// Handle Right arrow key
fn handle_right_arrow(app: &mut App) -> EventAction {
    app.input.move_right();
    EventAction::Continue
}

/// Handle Home key
fn handle_home(app: &mut App) -> EventAction {
    app.input.move_home();
    EventAction::Continue
}

/// Handle End key
fn handle_end(app: &mut App) -> EventAction {
    app.input.move_end();
    EventAction::Continue
}

/// Navigate to previous input in history (older messages)
fn navigate_history_backward(app: &mut App) {
    if app.session_state.input_history.is_empty() {
        return;
    }

    match app.session_state.history_index {
        None => {
            // First time pressing up - save current input and go to latest history entry
            app.session_state.history_buffer = app.input.get().to_string();
            app.session_state.history_index = Some(app.session_state.input_history.len() - 1);
            app.input.set(&app.session_state.input_history[app.session_state.history_index.unwrap()]);
        }
        Some(idx) if idx > 0 => {
            // Go to older message
            app.session_state.history_index = Some(idx - 1);
            app.input.set(&app.session_state.input_history[idx - 1]);
        }
        Some(0) => {
            // Already at oldest, do nothing
        }
        _ => {}
    }
}

/// Navigate to next input in history (newer messages, or clear at end)
fn navigate_history_forward(app: &mut App) {
    match app.session_state.history_index {
        Some(idx) if idx < app.session_state.input_history.len() - 1 => {
            // Go to newer message
            app.session_state.history_index = Some(idx + 1);
            app.input.set(&app.session_state.input_history[idx + 1]);
        }
        Some(_) => {
            // At newest - restore draft buffer and exit history mode
            app.session_state.history_index = None;
            app.input.set(&app.session_state.history_buffer);
        }
        None => {
            // Not in history mode, do nothing
        }
    }
}

/// Handle Up arrow (navigate history or scroll chat)
fn handle_up_arrow(app: &mut App) -> EventAction {
    // If not scrolling chat (input is focused), navigate history
    if !app.ui_state.chat_state.is_manually_scrolling() && !app.session_state.input_history.is_empty() {
        navigate_history_backward(app);
        return EventAction::Continue;
    }
    // Otherwise scroll chat up (shows older messages)
    app.scroll_up(1);
    EventAction::Continue
}

/// Handle Down arrow (navigate history or scroll chat)
fn handle_down_arrow(app: &mut App) -> EventAction {
    // If not scrolling chat (input is focused), navigate history
    if !app.ui_state.chat_state.is_manually_scrolling() && !app.session_state.input_history.is_empty() {
        navigate_history_forward(app);
        return EventAction::Continue;
    }
    // Otherwise scroll chat down (shows newer messages)
    app.scroll_down(1);
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

/// Handle Tab key - no-op (modes removed)
fn handle_tab(_app: &mut App, _modifiers: KeyModifiers) -> EventAction {
    EventAction::Continue
}

/// Handle BackTab - no-op (modes removed)
fn handle_backtab(_app: &mut App) -> EventAction {
    EventAction::Continue
}
