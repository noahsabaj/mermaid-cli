use crate::node::{BorderStyle, Line, Span, UiNode};
use crate::theme::{StyleToken, Theme, ThemeToken};
use crate::wrap::truncate_to_cells;
use mermaid_domain::RewindCandidate;

#[derive(Debug, Clone)]
pub struct RewindPickerProps<'a> {
    pub theme: &'a Theme,
    pub candidates: &'a [RewindCandidate],
    pub cursor: usize,
    pub height: usize,
}

#[must_use]
pub fn build_rewind_picker_view(props: RewindPickerProps<'_>) -> UiNode {
    let title =
        "Rewind — fork at an earlier message · ↑↓ navigate · Enter fork · Esc cancel".to_string();
    let inner_height = props.height.saturating_sub(2);
    let visible = inner_height.min(10);
    let start = if props.cursor >= visible {
        props.cursor + 1 - visible
    } else {
        0
    };

    let rows: Vec<Line> = props
        .candidates
        .iter()
        .enumerate()
        .skip(start)
        .take(visible)
        .map(|(i, candidate)| {
            let highlighted = i == props.cursor;
            let prefix = if highlighted { " > " } else { "   " };
            let excerpt = truncate_to_cells(&candidate.excerpt, 64);
            let meta = format!("  (#{} back)", i + 1);
            let excerpt_style = if highlighted {
                StyleToken::new()
                    .fg(ThemeToken::TextPrimary)
                    .bg(ThemeToken::TextDisabled)
                    .bold()
            } else {
                StyleToken::new().fg(ThemeToken::TextPrimary)
            };
            let meta_style = if highlighted {
                StyleToken::new()
                    .fg(ThemeToken::TextDisabled)
                    .bg(ThemeToken::TextDisabled)
                    .bold()
            } else {
                StyleToken::new().fg(ThemeToken::TextDisabled)
            };
            Line::from(vec![
                Span::raw(prefix),
                Span::styled(excerpt, excerpt_style),
                Span::styled(meta, meta_style),
            ])
        })
        .collect();

    UiNode::vertical(vec![UiNode::text(rows)], vec![])
        .with_border(BorderStyle::Plain, Some(ThemeToken::Border))
        .with_title(title)
}
