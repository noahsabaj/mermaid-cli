//! UI state management
//!
//! Visual presentation and widget states.

use ratatui::layout::{Constraint, Direction, Flex, Layout, Rect};
use ratatui::text::Line;
use rustc_hash::FxHashMap;

use crate::tui::theme::Theme;
use crate::tui::widgets::{ChatState, InputState};

/// Cache key for the main layout — all dimensions that, if changed,
/// invalidate the cached `Vec<Rect>`. Step 5e added `bottom_height`
/// (palette grows the bottom region from 2 lines to ~10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LayoutKey {
    width: u16,
    height: u16,
    input_height: u16,
    status_line_height: u16,
    attachment_height: u16,
    bottom_height: u16,
}

/// Cache for layout calculations to avoid recomputation each frame.
pub struct LayoutCache {
    cached: Option<(LayoutKey, Vec<Rect>)>,
}

impl LayoutCache {
    fn new() -> Self {
        Self { cached: None }
    }

    pub fn get_main_layout(
        &mut self,
        area: Rect,
        input_height: u16,
        status_line_height: u16,
        attachment_height: u16,
        bottom_height: u16,
    ) -> Vec<Rect> {
        let key = LayoutKey {
            width: area.width,
            height: area.height,
            input_height,
            status_line_height,
            attachment_height,
            bottom_height,
        };
        if let Some((cached_key, ref rects)) = self.cached
            && cached_key == key
        {
            return rects.clone();
        }

        let layout = Layout::default()
            .direction(Direction::Vertical)
            .margin(0)
            .spacing(0)
            .flex(Flex::Start)
            .constraints([
                Constraint::Min(10),
                Constraint::Length(status_line_height),
                Constraint::Length(attachment_height),
                Constraint::Length(input_height),
                Constraint::Length(bottom_height),
            ])
            .split(area);

        let layout_vec = layout.to_vec();
        self.cached = Some((key, layout_vec.clone()));
        layout_vec
    }
}

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
    /// Selected row in the slash-command palette (visible whenever the
    /// input starts with `/`). Indexes into the FILTERED list, not the
    /// full registry — reset to 0 whenever the filter changes so a
    /// shrinking result list can't leave the index out-of-bounds.
    pub palette_selected_index: usize,
    /// Layout cache (avoids recomputation each frame)
    pub layout_cache: LayoutCache,
    /// Cached parsed markdown per message: (message_index, content_hash) -> parsed lines
    pub markdown_cache: FxHashMap<u64, Vec<Line<'static>>>,
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
            palette_selected_index: 0,
            layout_cache: LayoutCache::new(),
            markdown_cache: FxHashMap::default(),
        }
    }
}

impl Default for UIState {
    fn default() -> Self {
        Self::new()
    }
}
