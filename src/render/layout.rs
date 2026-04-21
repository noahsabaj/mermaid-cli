//! Vertical layout computation.
//!
//! Pure — given the `State` and the available `Rect`, returns the
//! sub-rects for chat / attachments / input / status. Never mutates.
//! Called once per render pass at the top of `render()`.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use unicode_width::UnicodeWidthChar;

use crate::domain::State;

/// Layout zones, in top-to-bottom reading order.
#[derive(Debug)]
pub struct Zones {
    pub chat: Rect,
    pub attachments: Rect,
    pub input: Rect,
    pub status: Rect,
}

impl Zones {
    /// Compute layout for `area`, factoring in input buffer length
    /// (so the input box auto-grows), pending attachments, and
    /// status line presence.
    pub fn for_state(area: Rect, state: &State) -> Self {
        let input_lines = estimate_input_lines(&state.ui.input_buffer, area.width);
        let input_height = (input_lines + 2).min(7) as u16; // borders + cap

        let attachment_height = if state.ui.attachments.is_empty() { 0 } else { 1 };
        let status_height = if state.status.is_some() { 1 } else { 0 };

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(attachment_height),
                Constraint::Length(input_height),
                Constraint::Length(status_height),
            ])
            .split(area);

        Self {
            chat: chunks[0],
            attachments: chunks[1],
            input: chunks[2],
            status: chunks[3],
        }
    }
}

/// Estimate how many display rows the input buffer takes up inside a
/// `width`-wide box (with 2 cells of border padding). CJK and emoji
/// count as 2 cells; control chars count as 0; newlines are explicit
/// wraps. Caps at 5 so the box can't eat the whole screen.
fn estimate_input_lines(buffer: &str, width: u16) -> usize {
    if buffer.is_empty() {
        return 1;
    }
    let inner_width = width.saturating_sub(4) as usize;
    let mut lines = 1usize;
    let mut col = 0usize;
    for ch in buffer.chars() {
        let w = ch.width().unwrap_or(0);
        if ch == '\n' || (inner_width > 0 && col + w > inner_width) {
            lines += 1;
            col = if ch == '\n' { 0 } else { w };
        } else {
            col += w;
        }
    }
    lines.min(5)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Config;
    use std::path::PathBuf;

    fn state() -> State {
        State::new(
            Config::default(),
            PathBuf::from("/tmp/p"),
            "ollama/test".to_string(),
        )
    }

    #[test]
    fn empty_input_single_line() {
        assert_eq!(estimate_input_lines("", 80), 1);
    }

    #[test]
    fn linebreaks_count() {
        assert_eq!(estimate_input_lines("a\nb\nc", 80), 3);
    }

    #[test]
    fn wraps_at_inner_width() {
        // width 10 → inner width 6 → "abcdefg" should wrap at 6.
        assert_eq!(estimate_input_lines("abcdefg", 10), 2);
    }

    #[test]
    fn capped_at_five_lines() {
        assert_eq!(estimate_input_lines("\n\n\n\n\n\n\n", 80), 5);
    }

    #[test]
    fn zones_partition_area_without_overlap() {
        let area = Rect::new(0, 0, 80, 24);
        let zones = Zones::for_state(area, &state());
        assert!(zones.chat.height > 0);
        // Input box always visible.
        assert!(zones.input.height >= 3);
        // No attachment / status rows when empty.
        assert_eq!(zones.attachments.height, 0);
        assert_eq!(zones.status.height, 0);
    }

    #[test]
    fn status_reserves_one_row_when_present() {
        let mut s = state();
        s.status = Some(crate::domain::StatusLine {
            text: "hi".to_string(),
            kind: crate::domain::StatusKind::Info,
            shown_at: std::time::SystemTime::now(),
        });
        let area = Rect::new(0, 0, 80, 24);
        let zones = Zones::for_state(area, &s);
        assert_eq!(zones.status.height, 1);
    }
}
