use rustc_hash::FxHashMap;

use crate::node::Line;
use crate::theme::Theme;

/// Entry in the click map: maps a content line to an image in chat history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageClickTarget {
    pub message_index: usize,
    pub image_index: usize,
    pub image_number: Option<u64>,
}

/// One memoized chat-frame assembly.
#[derive(Debug, Clone)]
pub struct FrameMemo {
    pub key: u64,
    pub lines: Vec<Line>,
    pub click_map: Vec<(u16, ImageClickTarget)>,
}

/// State for the chat widget and viewport scrolling.
#[derive(Debug, Clone, Default)]
pub struct ChatState {
    scroll_offset: u16,
    is_user_scrolling: bool,
    pub image_click_map: Vec<(u16, ImageClickTarget)>,
    pub last_scroll_position: u16,
    pub last_chat_area: Option<(u16, u16, u16, u16)>,
    pub selection: Option<((usize, usize), (usize, usize))>,
    pub last_rendered_rows: Vec<String>,
    pub frame_memo: Option<FrameMemo>,
}

impl ChatState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn scroll_up(&mut self, lines: u16) {
        self.is_user_scrolling = true;
        self.scroll_offset = self.scroll_offset.saturating_add(lines);
    }

    pub fn scroll_down(&mut self, lines: u16) {
        if self.scroll_offset <= lines {
            self.scroll_offset = 0;
            self.is_user_scrolling = false;
        } else {
            self.scroll_offset -= lines;
        }
    }

    pub fn resume_auto_scroll(&mut self) {
        self.is_user_scrolling = false;
        self.scroll_offset = 0;
    }

    #[must_use]
    pub fn is_scrolling(&self) -> bool {
        self.is_user_scrolling
    }

    #[must_use]
    pub fn scroll_offset(&self) -> u16 {
        self.scroll_offset
    }

    pub fn set_scroll_offset(&mut self, offset: u16) {
        self.scroll_offset = offset;
        self.is_user_scrolling = offset > 0;
    }
}

/// Memoized stitched transcript.
#[derive(Debug, Clone)]
pub struct StitchedMemo {
    pub key: u64,
    pub messages: Vec<mermaid_model::models::ChatMessage>,
}

/// Pure presentation state and caches that live across frames.
#[derive(Debug, Clone)]
pub struct UiCache {
    pub chat: ChatState,
    pub wrapped_line_cache: FxHashMap<u64, Vec<Line>>,
    pub theme: Theme,
    pub applied_theme: Option<(mermaid_domain::ThemeChoice, bool)>,
    pub stitched: Option<StitchedMemo>,
    pub hostname: String,
    pub username: String,
    pub version: String,
    pub last_mouse_scroll_accum: i32,
    pub last_scroll_to_bottom_seq: u32,
}

impl UiCache {
    #[must_use]
    pub fn new(hostname: String, username: String, version: String) -> Self {
        Self {
            chat: ChatState::new(),
            wrapped_line_cache: FxHashMap::default(),
            theme: Theme::dark(),
            applied_theme: None,
            stitched: None,
            hostname,
            username,
            version,
            last_mouse_scroll_accum: 0,
            last_scroll_to_bottom_seq: 0,
        }
    }

    pub fn apply_theme_diff(&mut self, choice: mermaid_domain::ThemeChoice, no_color: bool) {
        let want = (choice, no_color);
        if self.applied_theme != Some(want) {
            self.theme = if no_color {
                Theme::plain()
            } else {
                match choice {
                    mermaid_domain::ThemeChoice::Dark => Theme::dark(),
                    mermaid_domain::ThemeChoice::Light => Theme::light(),
                }
            };
            self.wrapped_line_cache.clear();
            self.applied_theme = Some(want);
        }
    }

    pub fn update_scroll_from_state(&mut self, state: &mermaid_domain::State) {
        let pending = state.ui.mouse_scroll_accum - self.last_mouse_scroll_accum;
        if pending > 0 {
            self.chat.scroll_up(pending as u16);
        } else if pending < 0 {
            self.chat.scroll_down((-pending) as u16);
        }
        self.last_mouse_scroll_accum = state.ui.mouse_scroll_accum;

        if state.ui.scroll_to_bottom_seq != self.last_scroll_to_bottom_seq {
            self.chat.resume_auto_scroll();
            self.last_scroll_to_bottom_seq = state.ui.scroll_to_bottom_seq;
        }
    }
}
