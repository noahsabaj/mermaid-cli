//! Stateless TUI widgets.
//!
//! Each widget takes explicit props; the compose function in
//! `render::mod` pulls those props from `State` per frame. No widget
//! holds a reference to any god-object.
//!
//! The glyph vocabulary, one meaning each (no emoji, no dingbats -- CI bans
//! them; geometric shapes and box drawing are below the banned ranges):
//!
//! - `>`  the user's prompt
//! - `●`  the start of a block: an assistant reply, a tool call
//! - `⎿`  the result or continuation under a block
//! - `◦`  a live child row (a running agent)
//! - `√ ■ □ ⊘`  checklist states: done, in progress, pending, blocked
//! - `◐ ◓ ◑ ◒`  the spinner
//! - `·`  the only separator inside meta text
//!
//! Every meta surface -- footer, session header, spinner meta, agent rows,
//! checklist meta, run summary, system notices -- uses the theme's single
//! `text_meta` colour, undimmed.

mod approval;
mod chat;
mod conversation_list;
mod file_picker;
mod input;
mod model_picker;
mod plan_config;
mod question;
mod rewind_picker;
mod session_header;
mod slash_palette;
mod status;
mod status_line;
mod tasks;

pub use approval::ApprovalModalWidget;
pub use chat::{ChatState, ChatWidget, ImageClickTarget};
pub use conversation_list::ConversationListWidget;
pub use file_picker::FilePickerWidget;
pub use input::{InputState, InputWidget, rendered_row_count};
pub use model_picker::{MODEL_PICKER_HEIGHT, ModelPickerWidget};
pub use plan_config::{PLAN_CONFIG_HEIGHT, PLAN_CONFIG_ROWS, PlanConfigWidget, plan_config_rows};
pub use question::{QuestionModalWidget, question_modal_height};
pub use rewind_picker::RewindPickerWidget;
pub use session_header::{
    SESSION_HEADER_HEIGHT, abbreviate_home, build_session_header, session_header_visible,
};
pub use slash_palette::SlashPaletteWidget;
pub use status::StatusWidget;
pub use status_line::{AgentPanelRow, build_status_lines, spinner_glyph};
pub use tasks::{build_task_lines, tasks_height, tasks_visible};

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

use ratatui::style::Style;
use ratatui::text::{Line, Span};

/// Truncate a styled [`Line`] to `width` display cells, appending `…` when it overflows.
pub(super) fn truncate_line_to_cells(line: Line<'static>, width: usize) -> Line<'static> {
    let total_w: usize = line
        .spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum();
    if total_w <= width {
        return line;
    }
    if width == 0 {
        return Line::from(Vec::new());
    }
    let budget = width.saturating_sub(1);
    let mut out_spans = Vec::new();
    let mut current_w = 0usize;
    let mut last_style = Style::default();
    for span in line.spans {
        last_style = span.style;
        let mut buf = String::new();
        for ch in span.content.chars() {
            let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
            if current_w + cw > budget {
                break;
            }
            buf.push(ch);
            current_w += cw;
        }
        if !buf.is_empty() {
            out_spans.push(Span::styled(buf, span.style));
        }
        if current_w >= budget {
            break;
        }
    }
    out_spans.push(Span::styled("…", last_style));
    Line::from(out_spans)
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
    #[must_use]
    pub fn display_text(&self) -> &str {
        match self {
            Self::Idle => "Idle",
            Self::Sending => "Sending",
            Self::Thinking => "Thinking",
            Self::Streaming => "Streaming",
            Self::RunningTools => "Running tools",
            Self::Compacting => "Compacting",
            Self::Cancelling => "Cancelling",
        }
    }

    /// Convert from the reducer's typed turn state. `TurnState::Idle`
    /// maps to `Idle`; `Generating.phase` maps 1:1; every other
    /// active variant maps to `Streaming` (the status-line widget
    /// doesn't distinguish beyond the basic upstream/downstream
    /// arrow).
    #[must_use]
    pub fn from_turn(turn: &mermaid_domain::TurnState) -> Self {
        use mermaid_domain::{GenPhase, TurnState};
        match turn {
            TurnState::Idle => Self::Idle,
            TurnState::Generating { phase, .. } => match phase {
                GenPhase::Sending => Self::Sending,
                GenPhase::Thinking => Self::Thinking,
                GenPhase::Streaming => Self::Streaming,
            },
            TurnState::ExecutingTools { .. } => Self::RunningTools,
            TurnState::Compacting { .. } => Self::Compacting,
            TurnState::Cancelling { .. } => Self::Cancelling,
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
