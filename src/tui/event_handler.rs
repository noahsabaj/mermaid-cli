use anyhow::Result;
use base64::Engine as _;
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};

use crate::clipboard;
use crate::constants::UI_MOUSE_SCROLL_LINES;
use crate::tui::App;
use crate::utils::open_file;

/// Actions that can result from handling an event
#[derive(Debug, Clone)]
#[must_use]
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

/// Handle mouse events (scrolling and Ctrl+Click on images)
fn handle_mouse_event(
    app: &mut App,
    mouse: crossterm::event::MouseEvent,
    _viewport_height: u16,
) -> Result<EventAction> {
    match mouse.kind {
        MouseEventKind::ScrollUp => {
            app.scroll_up(UI_MOUSE_SCROLL_LINES); // Wheel up shows older messages (viewport moves up)
            Ok(EventAction::Continue)
        },
        MouseEventKind::ScrollDown => {
            app.scroll_down(UI_MOUSE_SCROLL_LINES); // Wheel down shows newer messages (viewport moves down)
            Ok(EventAction::Continue)
        },
        MouseEventKind::Down(crossterm::event::MouseButton::Left)
            if mouse.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            // Ctrl+Click: check pending attachments first (above input bar)
            if let Some(area_y) = app.ui_state.attachment_area_y
                && mouse.row == area_y
                && !app.attachment_state.is_empty()
            {
                // Click is in the pending attachment area - open the first attachment
                // (all images on one line; open whichever is there)
                if let Some(path) = app.attachment_state.last_temp_path() {
                    open_file(path);
                    app.set_status("Opening image preview...");
                }
                return Ok(EventAction::Continue);
            }

            // Ctrl+Click: check if an image indicator was clicked in chat history
            if let Some(target) = app.ui_state.chat_state.find_image_at_screen_pos(mouse.row) {
                let msg_idx = target.message_index;
                let img_idx = target.image_index;
                if let Some(msg) = app.session_state.messages.get(msg_idx)
                    && let Some(ref images) = msg.images
                    && let Some(base64_data) = images.get(img_idx)
                {
                    // Decode base64 and write to temp file, then open
                    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(base64_data)
                    {
                        // Detect format from PNG magic bytes
                        let ext = if bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
                            "png"
                        } else {
                            "jpeg"
                        };
                        let path = std::env::temp_dir()
                            .join(format!("mermaid-view-{}-{}.{}", msg_idx, img_idx, ext));
                        if std::fs::write(&path, &bytes).is_ok() {
                            open_file(&path);
                            app.set_status(format!("Opening Image #{}", img_idx + 1));
                        }
                    }
                }
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
    // Only handle key press events, not release or repeat.
    // On Windows, crossterm sends both Press and Release events for each keystroke,
    // which would cause duplicate character input without this filter.
    if key.kind != KeyEventKind::Press {
        return Ok(EventAction::Continue);
    }

    // Handle attachment focus mode (cursor is in the image attachment area above input)
    if app.ui_state.attachment_focused {
        let action = match key.code {
            KeyCode::Down | KeyCode::Enter => {
                // Exit attachment focus, return to input
                app.ui_state.attachment_focused = false;
                EventAction::Continue
            },
            KeyCode::Left => {
                if app.ui_state.selected_attachment > 0 {
                    app.ui_state.selected_attachment -= 1;
                }
                EventAction::Continue
            },
            KeyCode::Right => {
                if app.ui_state.selected_attachment + 1 < app.attachment_state.len() {
                    app.ui_state.selected_attachment += 1;
                }
                EventAction::Continue
            },
            KeyCode::Backspace | KeyCode::Delete => {
                let idx = app.ui_state.selected_attachment;
                if idx < app.attachment_state.len() {
                    app.attachment_state.remove_at(idx);
                    let remaining = app.attachment_state.len();
                    if remaining == 0 {
                        // No more attachments, exit focus mode
                        app.ui_state.attachment_focused = false;
                        app.ui_state.selected_attachment = 0;
                        app.set_status("All images removed");
                    } else {
                        // Clamp selection to valid range
                        if app.ui_state.selected_attachment >= remaining {
                            app.ui_state.selected_attachment = remaining - 1;
                        }
                        app.set_status(format!("Image removed ({} remaining)", remaining));
                    }
                }
                EventAction::Continue
            },
            KeyCode::Esc => {
                // Exit attachment focus
                app.ui_state.attachment_focused = false;
                EventAction::Continue
            },
            KeyCode::Char(c) => {
                // Any typing exits focus mode and inserts the character
                app.ui_state.attachment_focused = false;
                handle_char_input(app, c, key.modifiers)
            },
            _ => EventAction::Continue,
        };
        return Ok(action);
    }

    // Slash-command palette focus mode (input starts with `/`).
    // Intercepts ↑/↓/Tab/Esc to navigate the palette and complete
    // commands. All other keys (Enter, char input, Backspace, etc.)
    // fall through to the normal handlers — palette stays in sync via
    // the existing `showing_command_hints = input.starts_with('/')`
    // flag in render.rs.
    if app.input.get().starts_with('/')
        && let Some(action) = handle_palette_key(app, &key)
    {
        return Ok(action);
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
        _ => EventAction::Continue,
    };

    Ok(action)
}

/// Palette navigation: handle ↑/↓/Tab/Esc when input starts with `/`.
/// Returns `Some(action)` if the key was handled here; `None` to fall
/// through to the normal keyboard handlers (so Enter, char input, etc.
/// still work as usual).
fn handle_palette_key(app: &mut App, key: &crossterm::event::KeyEvent) -> Option<EventAction> {
    use crate::tui::slash_commands::filter_by_prefix;

    let typed = app
        .input
        .get()
        .trim_start_matches('/')
        .split_whitespace()
        .next()
        .unwrap_or("");
    let candidates = filter_by_prefix(typed);

    match key.code {
        KeyCode::Up => {
            if app.ui_state.palette_selected_index > 0 {
                app.ui_state.palette_selected_index -= 1;
            }
            Some(EventAction::Continue)
        },
        KeyCode::Down => {
            let max = candidates.len().saturating_sub(1);
            if app.ui_state.palette_selected_index < max {
                app.ui_state.palette_selected_index += 1;
            }
            Some(EventAction::Continue)
        },
        KeyCode::Tab => {
            // Complete the highlighted command name into the input.
            // Adds a trailing space so the user can immediately type
            // arguments (e.g. `/model <Tab>` → `/model | …` with the
            // cursor positioned to type the model name).
            if let Some(cmd) = candidates.get(app.ui_state.palette_selected_index) {
                let new_input = format!("/{} ", cmd.name);
                app.input.set(new_input);
                app.ui_state.palette_selected_index = 0;
            }
            Some(EventAction::Continue)
        },
        KeyCode::Esc => {
            // Dismiss palette by clearing the input. The normal Esc
            // semantics (stop generation, clear non-slash input) only
            // fire when input doesn't start with `/`.
            app.clear_input();
            app.ui_state.palette_selected_index = 0;
            Some(EventAction::Continue)
        },
        KeyCode::Enter => {
            // Complete-then-execute: replace the typed command word
            // with the highlighted selection (preserving any args the
            // user already typed), then fall through to the normal
            // Enter handler so the dispatcher runs. Without this, just
            // typing `/` and pressing Enter would dispatch an empty
            // command and silently flash "Unknown command:".
            //
            // If no candidate matches the typed prefix, leave input
            // alone — the dispatcher will surface its own clear
            // "Unknown command: <typed>" so the user knows why nothing
            // happened.
            if let Some(cmd) = candidates.get(app.ui_state.palette_selected_index) {
                let raw = app.input.get();
                let after_slash = raw.trim_start_matches('/');
                // Preserve everything from the first whitespace onward
                // (the user's args + spacing), only replacing the
                // command word itself.
                let rest = match after_slash.find(char::is_whitespace) {
                    Some(idx) => &after_slash[idx..],
                    None => "",
                };
                let new_input = format!("/{}{}", cmd.name, rest);
                app.input.set(new_input);
                app.ui_state.palette_selected_index = 0;
            }
            // Return None so the normal Enter handler runs and actually
            // dispatches the (now-completed) command. This is the
            // critical line — intercepting Enter here would close the
            // palette without executing anything.
            None
        },
        _ => None, // Fall through to normal handlers.
    }
}

/// Handle Escape key (stop generation or clear input)
fn handle_escape_key(app: &mut App) -> EventAction {
    if app.app_state.is_generating() {
        // Abort generation and save any partial response
        let (abort, partial) = app.abort_generation();
        if let Some(h) = abort {
            h.abort();
        }
        if !partial.is_empty() {
            use crate::models::MessageRole;
            app.add_message(MessageRole::Assistant, partial);
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

    // Check if this is a slash command (starts with '/')
    if app.input.get().starts_with('/') {
        // Commands only work when not generating
        if !app.app_state.is_generating() {
            let command = app.input.get().trim_start_matches('/').to_string();
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
/// Preserves formatting (including newlines) so pasted code keeps its structure.
/// Bracketed paste mode prevents newlines from triggering submission.
fn handle_paste(app: &mut App, text: &str) -> Result<EventAction> {
    // Strip carriage returns but preserve newlines for code formatting
    let cleaned = text.replace('\r', "");
    if !cleaned.is_empty() {
        // Reset history navigation when pasting
        if app.input.history_index.is_some() {
            app.input.history_index = None;
            app.input.history_buffer.clear();
        }
        app.input.insert_str(&cleaned);
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

    // Ctrl+V to paste image or text from clipboard
    if c == 'v' && modifiers == KeyModifiers::CONTROL {
        // Check if clipboard has image data (and model hasn't been marked as non-vision)
        if app.model_state.vision_supported != Some(false) && clipboard::has_image() {
            match clipboard::read_image_bytes() {
                Ok((bytes, format)) => {
                    let num = app.attachment_state.add(&bytes, format);
                    app.set_status(format!(
                        "Image #{} attached (Ctrl+O to preview, Backspace to remove)",
                        num
                    ));
                },
                Err(e) => {
                    app.set_status(format!("Failed to read clipboard image: {}", e));
                },
            }
        } else {
            // Fall back to text paste (preserve formatting)
            if let Ok(text) = clipboard::read_text() {
                let cleaned = text.replace('\r', "");
                if !cleaned.is_empty() {
                    if app.input.history_index.is_some() {
                        app.input.history_index = None;
                        app.input.history_buffer.clear();
                    }
                    app.input.insert_str(&cleaned);
                }
            }
        }
        return EventAction::Continue;
    }

    // Ctrl+O to preview last attached image
    if c == 'o' && modifiers == KeyModifiers::CONTROL {
        if let Some(path) = app.attachment_state.last_temp_path() {
            open_file(path);
            app.set_status("Opening image preview...");
        }
        return EventAction::Continue;
    }

    // Alt+T cycles reasoning depth (None → Low → Medium → High → Max → None)
    if c == 't' && modifiers == KeyModifiers::ALT {
        let new_level = app.model_state.cycle_reasoning();
        // Persist per-model so cycling on Sonnet doesn't bleed into the
        // next session with Ollama. Failure is non-fatal — the in-memory
        // state is still updated and the status message reflects it.
        let _ = crate::app::persist_reasoning_for_model(&app.model_state.model_id, new_level);
        app.set_status(format!("Reasoning: {}", new_level.as_str()));
        return EventAction::Continue;
    }

    // Normal character input (no modifiers or only SHIFT for uppercase)
    if modifiers.is_empty() || modifiers == KeyModifiers::SHIFT {
        // Reset history navigation when user starts typing
        if app.input.history_index.is_some() {
            app.input.history_index = None;
            app.input.history_buffer.clear();
        }
        app.input.insert(c);
        // Reset palette selection on every keystroke that mutates the
        // filter — prevents stale indices from pointing past the end of
        // a shrinking result list.
        app.ui_state.palette_selected_index = 0;
    }

    EventAction::Continue
}

/// Handle Backspace key
fn handle_backspace(app: &mut App) -> EventAction {
    if app.input.is_empty() && !app.attachment_state.is_empty() {
        app.attachment_state.remove_last();
        let remaining = app.attachment_state.len();
        if remaining > 0 {
            app.set_status(format!("Image removed ({} remaining)", remaining));
        } else {
            app.set_status("Image removed");
        }
    } else {
        app.input.backspace();
        // Reset palette selection on filter mutation (same reason as
        // the char-input path above).
        app.ui_state.palette_selected_index = 0;
    }
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
    if app.input.history.is_empty() {
        return;
    }

    match app.input.history_index {
        None => {
            // First time pressing up - save current input and go to latest history entry
            app.input.history_buffer = app.input.get().to_string();
            let idx = app.input.history.len() - 1;
            app.input.history_index = Some(idx);
            let entry = app.input.history[idx].clone();
            app.input.set(&entry);
        },
        Some(idx) if idx > 0 => {
            // Go to older message
            app.input.history_index = Some(idx - 1);
            let entry = app.input.history[idx - 1].clone();
            app.input.set(&entry);
        },
        Some(0) => {
            // Already at oldest, do nothing
        },
        _ => {},
    }
}

/// Navigate to next input in history (newer messages, or clear at end)
fn navigate_history_forward(app: &mut App) {
    match app.input.history_index {
        Some(idx) if idx < app.input.history.len() - 1 => {
            // Go to newer message
            app.input.history_index = Some(idx + 1);
            let entry = app.input.history[idx + 1].clone();
            app.input.set(&entry);
        },
        Some(_) => {
            // At newest - restore draft buffer and exit history mode
            app.input.history_index = None;
            let buf = app.input.history_buffer.clone();
            app.input.set(&buf);
        },
        None => {
            // Not in history mode, do nothing
        },
    }
}

/// Handle Up arrow (navigate history, enter attachment focus, or scroll chat)
fn handle_up_arrow(app: &mut App) -> EventAction {
    // If cursor is at position 0 (or input empty) and attachments exist, enter attachment focus
    if !app.ui_state.chat_state.is_manually_scrolling()
        && !app.attachment_state.is_empty()
        && app.input.cursor_position == 0
    {
        app.ui_state.attachment_focused = true;
        app.ui_state.selected_attachment = app.attachment_state.len().saturating_sub(1);
        return EventAction::Continue;
    }

    // If not scrolling chat (input is focused), navigate history
    if !app.ui_state.chat_state.is_manually_scrolling() && !app.input.history.is_empty() {
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
    if !app.ui_state.chat_state.is_manually_scrolling() && !app.input.history.is_empty() {
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
