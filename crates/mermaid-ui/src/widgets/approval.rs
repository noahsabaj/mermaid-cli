use crate::node::{BorderStyle, Line, Span, UiNode};
use crate::theme::{StyleToken, Theme, ThemeToken};
use crate::wrap::truncate_to_cells;

#[derive(Debug, Clone)]
pub struct ApprovalModalProps<'a> {
    pub theme: &'a Theme,
    pub title: String,
    pub body: &'a str,
    pub options: Vec<String>,
    pub selected_index: Option<usize>,
    pub accent: ThemeToken,
}

#[must_use]
pub fn build_approval_modal_view(props: ApprovalModalProps<'_>, inner_width: usize) -> UiNode {
    let mut lines: Vec<Line> = Vec::new();
    let body_width = inner_width.saturating_sub(4).max(8);

    for raw in props.body.lines() {
        lines.push(Line::from(Span::styled(
            truncate_to_cells(raw, body_width),
            StyleToken::new().fg(ThemeToken::TextPrimary),
        )));
    }
    lines.push(Line::from(""));

    for (idx, opt) in props.options.iter().enumerate() {
        let is_selected = props.selected_index == Some(idx);
        let style = if is_selected {
            StyleToken::new()
                .fg(ThemeToken::TextHighlight)
                .bold()
                .bg(ThemeToken::TextDisabled)
        } else {
            StyleToken::new().fg(ThemeToken::TextPrimary).bold()
        };
        lines.push(Line::from(Span::styled(format!("  {opt}"), style)));
    }

    UiNode::vertical(vec![UiNode::text(lines)], vec![])
        .with_border(BorderStyle::Plain, Some(props.accent))
        .with_title(props.title)
}
