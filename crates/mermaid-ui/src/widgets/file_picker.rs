use crate::node::{BorderStyle, Line, Span, UiNode};
use crate::theme::{StyleToken, Theme, ThemeToken};

const MAX_VISIBLE_ROWS: usize = 8;

#[derive(Debug, Clone)]
pub struct FilePickerProps<'a> {
    pub theme: &'a Theme,
    pub matches: &'a [String],
    pub selected_index: usize,
    pub loading: bool,
}

#[must_use]
pub fn build_file_picker_view(props: FilePickerProps<'_>) -> UiNode {
    let total = props.matches.len();
    let selected = props.selected_index.min(total.saturating_sub(1));
    let scroll_offset = if selected >= MAX_VISIBLE_ROWS {
        selected + 1 - MAX_VISIBLE_ROWS
    } else {
        0
    };
    let visible_end = (scroll_offset + MAX_VISIBLE_ROWS).min(total);

    let title = if total > MAX_VISIBLE_ROWS {
        format!(
            " Files ({}-{} of {})  ↑↓ navigate · Tab/Enter insert · Esc dismiss ",
            scroll_offset + 1,
            visible_end,
            total
        )
    } else {
        format!(" Files ({total})  ↑↓ navigate · Tab/Enter insert · Esc dismiss ")
    };

    if props.matches.is_empty() {
        let text = if props.loading {
            "  Scanning project files..."
        } else {
            "  No matching files"
        };
        let line = Line::from(vec![Span::styled(
            text,
            StyleToken::new().fg(ThemeToken::TextDisabled),
        )]);
        return UiNode::vertical(vec![UiNode::text(vec![line])], vec![])
            .with_border(BorderStyle::Plain, Some(ThemeToken::Border))
            .with_title(title);
    }

    let mut lines: Vec<Line> = Vec::with_capacity(MAX_VISIBLE_ROWS);
    for (offset, path) in props.matches[scroll_offset..visible_end].iter().enumerate() {
        let absolute_index = scroll_offset + offset;
        let is_selected = absolute_index == selected;

        let (dir, name) = match path.rfind('/') {
            Some(_) if path.ends_with('/') => ("", path.as_str()),
            Some(idx) => path.split_at(idx + 1),
            None => ("", path.as_str()),
        };
        let dir_style = if is_selected {
            StyleToken::new()
                .fg(ThemeToken::TextSecondary)
                .bg(ThemeToken::TextDisabled)
        } else {
            StyleToken::new().fg(ThemeToken::TextSecondary)
        };
        let name_style = if is_selected {
            StyleToken::new()
                .fg(ThemeToken::TextHighlight)
                .bg(ThemeToken::TextDisabled)
                .bold()
        } else {
            StyleToken::new().fg(ThemeToken::TextPrimary)
        };
        lines.push(Line::from(vec![
            Span::styled("  ", dir_style),
            Span::styled(dir.to_string(), dir_style),
            Span::styled(name.to_string(), name_style),
        ]));
    }

    UiNode::vertical(vec![UiNode::text(lines)], vec![])
        .with_border(BorderStyle::Plain, Some(ThemeToken::Border))
        .with_title(title)
}
