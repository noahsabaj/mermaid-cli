use crossterm::event::{KeyCode, KeyEvent};

/// Handle input processing
pub struct InputHandler {
    // Future: Add input history, autocomplete, etc.
}

impl InputHandler {
    pub fn new() -> Self {
        Self {}
    }

    /// Process a key event
    pub fn handle_key(&self, key: KeyEvent) -> InputAction {
        match key.code {
            KeyCode::Enter => InputAction::Submit,
            KeyCode::Esc => InputAction::Cancel,
            KeyCode::Char(c) => InputAction::Insert(c),
            KeyCode::Backspace => InputAction::Delete,
            KeyCode::Up => InputAction::HistoryPrev,
            KeyCode::Down => InputAction::HistoryNext,
            _ => InputAction::None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum InputAction {
    Submit,
    Cancel,
    Insert(char),
    Delete,
    HistoryPrev,
    HistoryNext,
    None,
}