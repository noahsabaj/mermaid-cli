//! Stateless TUI widgets.
//!
//! Each widget takes explicit props; the compose function in
//! `render::mod` pulls those props from `State` per frame. No widget
//! holds a reference to any god-object.

mod approval;
mod chat;
mod conversation_list;
mod input;
mod question;
mod slash_palette;
mod status;
mod status_line;

pub use approval::ApprovalModalWidget;
pub use chat::{ChatState, ChatWidget, ImageClickTarget};
pub use conversation_list::ConversationListWidget;
pub use input::{InputState, InputWidget};
pub use question::{QuestionModalWidget, question_modal_height};
pub use slash_palette::SlashPaletteWidget;
pub use status::StatusWidget;
pub use status_line::build_status_lines;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Truncate `s` to `width` display cells, appending `…` when it doesn't fit.
/// Cell-accurate (CJK/emoji safe) so the result never exceeds `width` — unlike a
/// `chars().count()` guard + byte-index cut, which under-counts wide glyphs (48
/// CJK chars = 96 cells slips through) and slices on a byte boundary.
pub(super) fn truncate_to_cells(s: &str, width: usize) -> String {
    if UnicodeWidthStr::width(s) <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let budget = width - 1; // leave a cell for the ellipsis
    let mut out = String::new();
    let mut w = 0usize;
    for ch in s.chars() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if w + cw > budget {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

/// Local-to-render-layer generation phase enum. The compose function
/// converts from `domain::TurnState` + `domain::GenPhase` into one of
/// these four states; widgets render off this local view so they
/// don't need to pattern-match the full domain enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenerationStatus {
    Idle,
    Sending,
    Thinking,
    Streaming,
    RunningTools,
    Compacting,
    Cancelling,
}

impl GenerationStatus {
    pub fn display_text(&self) -> &str {
        match self {
            GenerationStatus::Idle => "Idle",
            GenerationStatus::Sending => "Sending",
            GenerationStatus::Thinking => "Thinking",
            GenerationStatus::Streaming => "Streaming",
            GenerationStatus::RunningTools => "Running tools",
            GenerationStatus::Compacting => "Compacting",
            GenerationStatus::Cancelling => "Cancelling",
        }
    }

    /// Convert from the reducer's typed turn state. `TurnState::Idle`
    /// maps to `Idle`; `Generating.phase` maps 1:1; every other
    /// active variant maps to `Streaming` (the status-line widget
    /// doesn't distinguish beyond the basic upstream/downstream
    /// arrow).
    pub fn from_turn(turn: &crate::domain::TurnState) -> Self {
        use crate::domain::{GenPhase, TurnState};
        match turn {
            TurnState::Idle => GenerationStatus::Idle,
            TurnState::Generating { phase, .. } => match phase {
                GenPhase::Sending => GenerationStatus::Sending,
                GenPhase::Thinking => GenerationStatus::Thinking,
                GenPhase::Streaming => GenerationStatus::Streaming,
            },
            TurnState::ExecutingTools { .. } => GenerationStatus::RunningTools,
            TurnState::Compacting { .. } => GenerationStatus::Compacting,
            TurnState::Cancelling { .. } => GenerationStatus::Cancelling,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::truncate_to_cells;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn fits_within_width_returns_unchanged() {
        assert_eq!(truncate_to_cells("hello", 10), "hello");
        assert_eq!(truncate_to_cells("hello", 5), "hello");
    }

    #[test]
    fn ascii_truncates_with_ellipsis_within_budget() {
        let out = truncate_to_cells("hello world", 5);
        assert_eq!(out, "hell…");
        assert!(UnicodeWidthStr::width(out.as_str()) <= 5);
    }

    #[test]
    fn wide_glyphs_never_exceed_budget() {
        // Each CJK char is 2 cells. The old `chars().count()` guard let a
        // 48-char (96-cell) title slip through a "48" budget and overflow its
        // row; cell-accurate truncation caps the display width.
        let cjk = "你好世界你好世界"; // 8 chars = 16 cells
        let out = truncate_to_cells(cjk, 6);
        assert!(
            UnicodeWidthStr::width(out.as_str()) <= 6,
            "width exceeded budget: {out:?}"
        );
        assert!(out.ends_with('…'));
    }

    #[test]
    fn zero_width_is_empty() {
        assert_eq!(truncate_to_cells("anything", 0), "");
    }
}
