use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Style,
    widgets::{Block, Borders, Paragraph, StatefulWidget, Widget},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::render::theme::Theme;

/// State for the input widget
#[derive(Debug, Clone)]
pub struct InputState {
    /// Cursor position in the input string
    pub cursor_position: usize,
}

impl InputState {
    /// Create a new input state
    #[must_use]
    pub fn new() -> Self {
        Self { cursor_position: 0 }
    }

    /// Calculate cursor position for wrapped text.
    ///
    /// `content_width` is in **display cells**. Returns `(row, col)` where
    /// `col` is also in display cells — required because `Frame::set_cursor_
    /// position` is cell-based, not byte-based. CJK / emoji input previously
    /// mispositioned the cursor because the column was returned in bytes.
    ///
    /// Uses the shared `layout_rows` helper so the wrapping decisions match
    /// `wrap_input_with_prompt` exactly (the two would silently drift
    /// otherwise — `cursor_and_wrap_agree_on_line_structure` guards this).
    #[must_use]
    pub fn calculate_cursor_position(
        input: &str,
        cursor_pos: usize,
        content_width: usize,
    ) -> (u16, u16) {
        let cursor_pos = cursor_pos.min(input.len());

        if content_width < 3 || input.is_empty() {
            return (0, 0);
        }

        // Available cells per line after the 2-cell prefix ("> " or "  ")
        let line_width = content_width.saturating_sub(2);
        if line_width == 0 {
            return (0, 0);
        }

        let rows = layout_rows(input, line_width);
        for (idx, row) in rows.iter().enumerate() {
            let content_end = row.start + row.len;
            let gap_end = content_end + row.gap;
            let is_last = idx + 1 == rows.len();

            // Cursor belongs to this row if it falls within the row chars or
            // the whitespace/newline gap after it, or if this is the last row.
            if cursor_pos < gap_end || is_last {
                // Cap at the row's content so trailing/gap whitespace doesn't
                // overflow past the visible line.
                let cursor_byte_in_line = cursor_pos.saturating_sub(row.start).min(row.len);
                let line_text = &input[row.start..content_end];
                let col_cells = line_text[..cursor_byte_in_line.min(line_text.len())].width();
                return (idx as u16, col_cells as u16);
            }
        }
        (0, 0)
    }
}

impl Default for InputState {
    fn default() -> Self {
        Self::new()
    }
}

/// Props for `InputWidget`. The slash-command palette is rendered
/// separately as `SlashPaletteWidget` in the bottom region (see
/// `render.rs`); this widget just draws the bordered input box.
pub struct InputWidget<'a> {
    pub input: &'a str,
    /// True when a slash command is in flight (input starts with `/`).
    /// Drives the warning-yellow border color so the user has a visual
    /// cue that they're in command-entry mode.
    pub showing_command_hints: bool,
    pub theme: &'a Theme,
    /// Reasoning is currently enabled (any non-`None` level). Drives the
    /// cyan/sage border color cue.
    pub reasoning_active: bool,
    /// A first Ctrl+C armed the exit confirmation; show the second-press
    /// hint in the box title while the window is open (idle-state surface —
    /// the busy-state hint lives in the status line).
    pub exit_armed: bool,
    /// A first idle Esc armed the double-Esc rewind; show the second-press
    /// hint in the box title while the window is open. Exit arming wins
    /// when both are somehow live (quitting is the more consequential act).
    pub rewind_armed: bool,
}

impl<'a> StatefulWidget for InputWidget<'a> {
    type State = InputState;

    fn render(self, area: Rect, buf: &mut Buffer, _state: &mut Self::State) {
        let input_style = Style::new().fg(self.theme.colors.text_primary.to_color());

        // Manually wrap input text with proper indentation (Claude Code style)
        // First line: "> text"
        // Continuation lines: "  text" (2 spaces to align with first line content)
        // Always show "> " prompt, even when input is empty
        let input_text = {
            let width = area.width.saturating_sub(2) as usize; // Account for top/bottom borders
            wrap_input_with_prompt(self.input, width)
        };

        // Border color priority: command-entry mode wins (yellow),
        // then reasoning-enabled (cyan), then default gray.
        let border_color = if self.showing_command_hints {
            self.theme.colors.warning.to_color()
        } else if self.reasoning_active {
            // Mermaid sage blue - same as the path color in status bar
            self.theme.colors.info.to_color() // cyan
        } else {
            self.theme.colors.border.to_color() // gray
        };

        let block = if self.showing_command_hints {
            Block::default()
                .borders(Borders::TOP | Borders::BOTTOM)
                .border_style(Style::new().fg(border_color))
                .title(" Enter Command ")
        } else if self.exit_armed {
            Block::default()
                .borders(Borders::TOP | Borders::BOTTOM)
                .border_style(Style::new().fg(border_color))
                .title(" press ctrl+c again to exit ")
        } else if self.rewind_armed {
            Block::default()
                .borders(Borders::TOP | Borders::BOTTOM)
                .border_style(Style::new().fg(border_color))
                .title(" esc again to rewind ")
        } else {
            Block::default()
                .borders(Borders::TOP | Borders::BOTTOM)
                .border_style(Style::new().fg(border_color))
        };

        let input = Paragraph::new(input_text).style(input_style).block(block);

        input.render(area, buf);

        // Note: Cursor positioning is handled in the main render loop after all widgets are rendered
        // The Frame::set_cursor_position() is called there with the calculated position
    }
}

/// Given a tail of input and a max line width (in **display cells**, not
/// bytes), return the byte offset where this line should end.
///
/// Walks `remaining` char-by-char accumulating `UnicodeWidthChar::width`
/// so CJK / emoji break at the visual edge instead of after ~1/3 of the
/// space (the byte length of multi-byte chars exceeds their cell width).
/// Prefers a whitespace break within the accepted range; falls back to a
/// hard break at the char boundary if no whitespace exists. Always makes
/// progress: if even the first character exceeds `line_width`, returns
/// the byte offset *after* it so the caller can't infinite-loop.
///
/// Shared between `InputState::calculate_cursor_position` and
/// `wrap_input_with_prompt` so both make identical wrapping decisions.
fn find_line_break(remaining: &str, line_width: usize) -> usize {
    if remaining.is_empty() {
        return 0;
    }

    // Walk chars, accumulating display width, to find the byte offset at
    // which the running cell-count would exceed `line_width`. If the whole
    // string fits, we're done.
    let mut acc_width = 0usize;
    let mut hard_break = remaining.len();
    for (byte_idx, ch) in remaining.char_indices() {
        let ch_width = ch.width().unwrap_or(0);
        if acc_width + ch_width > line_width {
            hard_break = byte_idx;
            break;
        }
        acc_width += ch_width;
    }

    if hard_break == remaining.len() {
        return remaining.len();
    }

    // If the very first character is wider than the entire line (e.g. a
    // double-width emoji on a 1-cell viewport), force progress by taking
    // exactly one char — otherwise the caller loops forever.
    if hard_break == 0 {
        return remaining
            .char_indices()
            .nth(1)
            .map(|(idx, _)| idx)
            .unwrap_or(remaining.len());
    }

    // Prefer a whitespace break within the accepted byte range. Advance past
    // the whitespace char by its UTF-8 length so the returned offset is always
    // a char boundary — `char::is_whitespace` matches multibyte spaces (NBSP
    // U+00A0 = 2 bytes, ideographic space U+3000 = 3 bytes, U+2028, …) that a
    // naive `pos + 1` would split mid-codepoint, panicking the renderer on the
    // subsequent slice. For 1-byte ASCII whitespace this is identical to the
    // old `pos + 1`.
    remaining[..hard_break]
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map(|(pos, c)| pos + c.len_utf8())
        .unwrap_or(hard_break)
}

/// One rendered row's span within the original input, in bytes.
///
/// `start..start+len` is the row's visible text. `gap` is the
/// whitespace/newline consumed after it before the next row begins (trimmed
/// inter-word whitespace from a soft wrap, plus the `\n` byte of a hard
/// break). Shared by `wrap_input_with_prompt` and `calculate_cursor_position`
/// so they never disagree on line structure.
struct RowSpan {
    start: usize,
    len: usize,
    gap: usize,
}

/// Lay `input` out into rendered rows at `line_width` (display cells).
/// Explicit `\n` forces a new row (so pasted/Ctrl+J newlines render as
/// real lines); each resulting segment is then soft-wrapped on width via
/// `find_line_break`. A trailing newline yields a final empty row.
fn layout_rows(input: &str, line_width: usize) -> Vec<RowSpan> {
    let mut rows: Vec<RowSpan> = Vec::new();
    if input.is_empty() {
        return rows;
    }
    let total = input.len();
    let mut seg_start = 0usize;
    loop {
        let seg_end = match input[seg_start..].find('\n') {
            Some(rel) => seg_start + rel,
            None => total,
        };
        let segment = &input[seg_start..seg_end];

        // Soft-wrap this newline-free segment into >=1 rows.
        let mut local = 0usize;
        loop {
            let rem = &segment[local..];
            let bp = find_line_break(rem, line_width);
            let after = &rem[bp..];
            let ws_gap = after.len() - after.trim_start().len();
            rows.push(RowSpan {
                start: seg_start + local,
                len: bp,
                gap: ws_gap,
            });
            local += bp + ws_gap;
            if local >= segment.len() {
                break;
            }
        }

        if seg_end >= total {
            break;
        }
        // A '\n' follows: count it in the last row's gap so cursor math lands
        // the caret on the correct side of the break.
        if let Some(last) = rows.last_mut() {
            last.gap += 1;
        }
        seg_start = seg_end + 1;
        if seg_start == total {
            // Trailing newline → a final empty row.
            rows.push(RowSpan {
                start: seg_start,
                len: 0,
                gap: 0,
            });
            break;
        }
    }
    rows
}

/// Wrap input text with "> " prefix on the first line and "  " on
/// continuation lines (Claude Code style). Always returns at least "> ",
/// even when input is empty. Embedded newlines render as real rows.
fn wrap_input_with_prompt(input: &str, width: usize) -> String {
    if width < 3 {
        // Not enough space for "> " prefix
        return input.to_string();
    }
    if input.is_empty() {
        return String::from("> ");
    }

    // First line and continuation lines both reserve 2 chars for their
    // respective prefix ("> " or "  "), so they share the same line width.
    let line_width = width.saturating_sub(2);

    let mut result = String::new();
    for (idx, row) in layout_rows(input, line_width).iter().enumerate() {
        let text = input[row.start..row.start + row.len].trim_end();
        if idx == 0 {
            result.push_str("> ");
        } else {
            result.push('\n');
            result.push_str("  ");
        }
        result.push_str(text);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parity: for every byte offset in `input`, `calculate_cursor_position`
    /// must return a (row, col) that lands in the same visual line emitted
    /// by `wrap_input_with_prompt`. Catches silent drift between the two
    /// functions going forward.
    #[test]
    fn cursor_and_wrap_agree_on_line_structure() {
        let inputs = [
            "hello world",
            "the quick brown fox jumps over the lazy dog",
            "nospacesinthislonginputthatmusthardbreak",
            "mixed short and verylongcontiguoustoken here",
            "leading  double  spaces  between  words",
            "",
            // CJK inputs: each char is 3 bytes / 2 display cells. The wrap
            // logic must agree on line structure across both functions.
            "你好世界",
            "你好 world 世界",
            "abc你好def世界ghi",
            // Embedded newlines (pasted / Ctrl+J): hard line breaks.
            "first line\nsecond line",
            "para one\n\npara two",
            "trailing newline\n",
            "\nleading newline",
        ];
        let content_width = 20usize;
        for input in inputs {
            let wrapped = wrap_input_with_prompt(input, content_width);

            // Strip prefixes to count content lines (first line "> ",
            // continuation lines "  "). This yields one vec per rendered
            // line holding the post-prefix content.
            let rendered_lines: Vec<String> = wrapped
                .split('\n')
                .enumerate()
                .map(|(i, line)| {
                    let prefix = if i == 0 { "> " } else { "  " };
                    line.strip_prefix(prefix).unwrap_or(line).to_string()
                })
                .collect();

            // For each byte offset in the input, ask the cursor function
            // which (row, col) it belongs to, then assert the row index is
            // in range for the wrapped text.
            for cursor_pos in 0..=input.len() {
                if !input.is_char_boundary(cursor_pos) {
                    continue;
                }
                let (row, _col) =
                    InputState::calculate_cursor_position(input, cursor_pos, content_width);
                assert!(
                    (row as usize) < rendered_lines.len().max(1),
                    "cursor row {} out of wrap range ({} lines) for input {:?} at byte {}",
                    row,
                    rendered_lines.len(),
                    input,
                    cursor_pos,
                );
            }
        }
    }

    #[test]
    fn find_line_break_whitespace_preferred() {
        assert_eq!(find_line_break("hello world foo", 10), 6);
    }

    #[test]
    fn find_line_break_hard_break_without_whitespace() {
        assert_eq!(find_line_break("abcdefghijklmno", 5), 5);
    }

    #[test]
    fn find_line_break_respects_char_boundary() {
        // 3-byte CJK chars: each is 3 bytes / 2 display cells. With
        // `line_width = 4` cells we fit exactly two CJK chars (4 cells,
        // 6 bytes). Old byte-based code returned 3 (only the first char),
        // wasting half the line.
        let s = "你好";
        assert_eq!(find_line_break(s, 4), 6);
    }

    #[test]
    fn find_line_break_uses_display_width_for_cjk() {
        // Cell width of "你好世界abc" = 4*2 + 3 = 11 cells; `line_width=10`
        // fits "你好世界ab" (10 cells, 14 bytes) and breaks before "c".
        let s = "你好世界abc";
        assert_eq!(find_line_break(s, 10), 14);
    }

    #[test]
    fn find_line_break_whole_remaining_fits() {
        assert_eq!(find_line_break("short", 100), "short".len());
    }

    #[test]
    fn find_line_break_makes_progress_when_first_char_overflows() {
        // Double-width char on a 1-cell viewport: must still consume the
        // char (return offset 3) so the wrap loop can't spin forever.
        assert_eq!(find_line_break("你hello", 1), 3);
    }

    #[test]
    fn find_line_break_multibyte_whitespace_is_char_boundary() {
        // Regression: `char::is_whitespace` matches multibyte spaces (NBSP
        // U+00A0 = 2 bytes, ideographic space U+3000 = 3 bytes, U+2028,
        // U+202F …). A naive `pos + 1` break offset lands mid-codepoint and
        // the caller's `&rem[bp..]` slice panics the whole renderer. The
        // break must always be a char boundary, and the full wrap path must
        // not panic.
        for s in [
            "aaaa\u{00A0}bbbbbbbbbbbbbbbb",
            "\u{3000}\u{3000}wwwwwwwwwwwwwww",
            "word\u{2028}word\u{202F}wordwordword",
        ] {
            for width in 1..=20 {
                let bp = find_line_break(s, width);
                assert!(
                    s.is_char_boundary(bp),
                    "break {bp} not a char boundary in {s:?} at width {width}",
                );
                let _ = &s[bp..]; // must not panic
            }
            let _ = wrap_input_with_prompt(s, 8);
        }
    }

    #[test]
    fn wrap_renders_embedded_newlines_as_rows() {
        // A pasted multi-line block must show as multiple rows, not a single
        // space-joined paragraph.
        assert_eq!(wrap_input_with_prompt("a\nb", 20), "> a\n  b");
        // Consecutive newlines keep the blank line.
        assert_eq!(wrap_input_with_prompt("a\n\nb", 20), "> a\n  \n  b");
        // A trailing newline yields an empty continuation row.
        assert_eq!(wrap_input_with_prompt("a\n", 20), "> a\n  ");
    }

    #[test]
    fn cursor_tracks_rows_across_newlines() {
        // "a\nb": byte 0=before a, 1=after a (on \n), 2=before b, 3=after b.
        assert_eq!(InputState::calculate_cursor_position("a\nb", 0, 20), (0, 0));
        assert_eq!(InputState::calculate_cursor_position("a\nb", 1, 20), (0, 1));
        assert_eq!(InputState::calculate_cursor_position("a\nb", 2, 20), (1, 0));
        assert_eq!(InputState::calculate_cursor_position("a\nb", 3, 20), (1, 1));
    }
}
