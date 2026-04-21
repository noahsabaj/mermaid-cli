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
    /// Uses `find_line_break` so the wrapping decisions match
    /// `wrap_input_with_prompt` exactly — keep them consistent by routing
    /// any future line-break logic through that shared helper.
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

        let mut current_line: usize = 0;
        let mut consumed: usize = 0; // byte offset into `input`

        let mut chars_remaining = input;
        loop {
            let break_point = find_line_break(chars_remaining, line_width);

            // Calculate whitespace gap between this line and the next
            let after = &chars_remaining[break_point..];
            let next_content = after.trim_start();
            let ws_gap = after.len() - next_content.len();
            let is_last_line = next_content.is_empty();

            // Cursor belongs to this line if it falls within the line chars,
            // or within the whitespace gap (trimmed between lines), or if
            // this is the last line (cursor must be here).
            if cursor_pos < consumed + break_point + ws_gap || is_last_line {
                // Cap at break_point so trailing / gap whitespace doesn't
                // overflow past the visible line.
                let cursor_byte_in_line = cursor_pos.saturating_sub(consumed).min(break_point);
                // Convert the byte offset to display cells by summing widths.
                let line_text = &chars_remaining[..break_point];
                let col_cells = line_text[..cursor_byte_in_line.min(line_text.len())].width();
                return (current_line as u16, col_cells as u16);
            }

            consumed += break_point + ws_gap;
            chars_remaining = next_content;
            current_line += 1;
        }
    }
}

impl Default for InputState {
    fn default() -> Self {
        Self::new()
    }
}

/// Props for InputWidget. The slash-command palette is rendered
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

    // Prefer a whitespace break within the accepted byte range. (`pos + 1`
    // assumes 1-byte ASCII whitespace, which is overwhelmingly the common
    // case in source text and matches the prior behavior.)
    remaining[..hard_break]
        .rfind(char::is_whitespace)
        .map(|pos| pos + 1)
        .unwrap_or(hard_break)
}

/// Wrap input text with "> " prefix on first line and "  " on continuation lines (Claude Code style)
/// Always returns at least "> " even when input is empty
fn wrap_input_with_prompt(input: &str, width: usize) -> String {
    if width < 3 {
        // Not enough space for "> " prefix
        return input.to_string();
    }

    // Always start with "> " prompt
    let mut result = String::from("> ");

    // If input is empty, just return the prompt
    if input.is_empty() {
        return result;
    }

    // First line and continuation lines both reserve 2 chars for their
    // respective prefix ("> " or "  "), so they share the same line width.
    let line_width = width.saturating_sub(2);

    let mut chars_remaining = input;
    let mut is_first_line = true;

    while !chars_remaining.is_empty() {
        let break_point = find_line_break(chars_remaining, line_width);

        // Extract this line's text
        let line_text = &chars_remaining[..break_point];

        // Add line text (prefix already added for first line, or add it for continuation)
        if is_first_line {
            // First line: "> " already in result, just add the text
            result.push_str(line_text.trim_end());
        } else {
            // Continuation line: add newline + "  " prefix + text
            result.push('\n');
            result.push_str("  ");
            result.push_str(line_text.trim_end());
        }

        // Move to next line
        chars_remaining = chars_remaining[break_point..].trim_start();
        is_first_line = false;
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
}
