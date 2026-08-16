use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::node::{BorderStyle, Line, Span, UiNode};
use crate::theme::{StyleToken, Theme, ThemeToken};

#[derive(Debug, Clone, Default)]
pub struct InputState {
    pub cursor_position: usize,
}

impl InputState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

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
        let line_width = content_width.saturating_sub(2);
        if line_width == 0 {
            return (0, 0);
        }
        let rows = layout_rows(input, line_width);
        for (idx, row) in rows.iter().enumerate() {
            let content_end = row.start + row.len;
            let gap_end = content_end + row.gap;
            let is_last = idx + 1 == rows.len();
            if cursor_pos < gap_end || is_last {
                let cursor_byte_in_line = cursor_pos.saturating_sub(row.start).min(row.len);
                let line_text = &input[row.start..content_end];
                let col_cells = line_text[..cursor_byte_in_line.min(line_text.len())].width();
                return (idx as u16, col_cells as u16);
            }
        }
        (0, 0)
    }
}

pub struct RowSpan {
    pub start: usize,
    pub len: usize,
    pub gap: usize,
}

pub fn layout_rows(input: &str, line_width: usize) -> Vec<RowSpan> {
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
        if let Some(last) = rows.last_mut() {
            last.gap += 1;
        }
        seg_start = seg_end + 1;
        if seg_start == total {
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

pub fn find_line_break(remaining: &str, line_width: usize) -> usize {
    if remaining.is_empty() {
        return 0;
    }
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
    if hard_break == 0 {
        return remaining
            .char_indices()
            .nth(1)
            .map(|(idx, _)| idx)
            .unwrap_or(remaining.len());
    }
    remaining[..hard_break]
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map(|(pos, c)| pos + c.len_utf8())
        .unwrap_or(hard_break)
}

#[must_use]
pub fn rendered_row_count(input: &str, content_width: usize) -> usize {
    if input.is_empty() {
        return 1;
    }
    let line_width = content_width.saturating_sub(2).max(1);
    layout_rows(input, line_width).len().max(1)
}

#[must_use]
pub fn wrap_input_with_prompt(input: &str, width: usize) -> Vec<Line> {
    if input.is_empty() {
        return vec![Line::from(vec![Span::styled(
            "> ",
            StyleToken::new().fg(ThemeToken::Brand),
        )])];
    }
    let line_width = width.saturating_sub(2).max(1);
    let rows = layout_rows(input, line_width);
    rows.into_iter()
        .enumerate()
        .map(|(idx, row)| {
            let prefix = if idx == 0 { "> " } else { "  " };
            let prefix_style = if idx == 0 {
                StyleToken::new().fg(ThemeToken::Brand)
            } else {
                StyleToken::new().fg(ThemeToken::TextPrimary)
            };
            let content = &input[row.start..row.start + row.len];
            Line::from(vec![
                Span::styled(prefix, prefix_style),
                Span::styled(
                    content.to_string(),
                    StyleToken::new().fg(ThemeToken::TextPrimary),
                ),
            ])
        })
        .collect()
}

#[derive(Debug, Clone)]
pub struct InputProps<'a> {
    pub input: &'a str,
    pub showing_command_hints: bool,
    pub theme: &'a Theme,
    pub reasoning_active: bool,
    pub exit_armed: bool,
    pub rewind_armed: bool,
    pub width: usize,
}

#[must_use]
pub fn build_input_view(props: InputProps<'_>) -> UiNode {
    let lines = wrap_input_with_prompt(props.input, props.width);
    let border_token = if props.showing_command_hints {
        ThemeToken::Warning
    } else if props.reasoning_active {
        ThemeToken::Info
    } else {
        ThemeToken::Border
    };

    let mut node = UiNode::vertical(vec![UiNode::text(lines)], vec![])
        .with_border(BorderStyle::Plain, Some(border_token));

    if props.showing_command_hints {
        node = node.with_title(" Enter Command ".to_string());
    } else if props.exit_armed {
        node = node.with_title(" press ctrl+c again to exit ".to_string());
    } else if props.rewind_armed {
        node = node.with_title(" esc again to rewind ".to_string());
    }

    node
}
