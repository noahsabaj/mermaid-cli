/// UI state management
///
/// Visual presentation and widget states.

use crate::tui::theme::Theme;
use crate::tui::widgets::{ChatState, InputState};

/// UI state - visual presentation and widget states
pub struct UIState {
    /// Chat widget state (scroll, scrolling flag)
    pub chat_state: ChatState,
    /// Input widget state (cursor position for display)
    pub input_state: InputState,
    /// UI theme
    pub theme: Theme,
    /// Selected message index (for navigation)
    pub selected_message: Option<usize>,
    /// Whether focus is in the attachment area (above input)
    pub attachment_focused: bool,
    /// Which attachment is selected when attachment_focused is true
    pub selected_attachment: usize,
    /// Attachment area rect from last render (for Ctrl+Click detection)
    pub attachment_area_y: Option<u16>,
}

impl UIState {
    /// Create a new UIState with default values
    pub fn new() -> Self {
        Self {
            chat_state: ChatState::default(),
            input_state: InputState::default(),
            theme: Theme::dark(),
            selected_message: None,
            attachment_focused: false,
            selected_attachment: 0,
            attachment_area_y: None,
        }
    }
}

impl Default for UIState {
    fn default() -> Self {
        Self::new()
    }
}
