use crate::node::{BorderStyle, Line, Span, UiNode};
use crate::theme::{StyleToken, Theme, ThemeToken};
use crate::wrap::truncate_to_cells;
use mermaid_domain::ConversationSummary;

#[derive(Debug, Clone)]
pub struct ConversationListProps<'a> {
    pub theme: &'a Theme,
    pub candidates: &'a [ConversationSummary],
    pub cursor: usize,
    pub height: usize,
}

#[must_use]
pub fn build_conversation_list_view(props: ConversationListProps<'_>) -> UiNode {
    let title = if props.candidates.is_empty() {
        "Load conversation — (none found)".to_string()
    } else {
        "Load conversation — ↑↓ navigate · Enter select · Esc cancel".to_string()
    };

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
        .map(|(i, summary)| {
            let highlighted = i == props.cursor;
            let prefix = if highlighted { " > " } else { "   " };
            let title = truncate_to_cells(&summary.title, 48);
            let meta = format!(
                "  ({} msg · {})",
                summary.message_count,
                short_timestamp(&summary.updated_at)
            );
            let title_style = if highlighted {
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
                Span::styled(title, title_style),
                Span::styled(meta, meta_style),
            ])
        })
        .collect();

    UiNode::vertical(vec![UiNode::text(rows)], vec![])
        .with_border(BorderStyle::Plain, Some(ThemeToken::Border))
        .with_title(title)
}

fn short_timestamp(rfc3339: &str) -> String {
    if rfc3339.len() >= 16 {
        let cut = rfc3339.floor_char_boundary(16);
        let mut s = rfc3339[..cut].to_string();
        if let Some(t_pos) = s.find('T') {
            s.replace_range(t_pos..t_pos + 1, " ");
        }
        s
    } else {
        rfc3339.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_timestamp_formats_rfc3339() {
        assert_eq!(
            short_timestamp("2026-04-21T14:30:12-04:00"),
            "2026-04-21 14:30"
        );
    }
}
