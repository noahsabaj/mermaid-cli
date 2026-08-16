use crate::node::{BorderStyle, Line, Span, UiNode};
use crate::theme::{StyleToken, Theme, ThemeToken};
use mermaid_domain::slash_commands::PaletteEntry;

const MAX_VISIBLE_ROWS: usize = 8;

pub struct SlashPaletteProps<'a> {
    pub theme: &'a Theme,
    pub entries: Vec<PaletteEntry<'a>>,
    pub selected_index: usize,
}

#[must_use]
pub fn build_slash_palette_view(props: SlashPaletteProps<'_>) -> UiNode {
    let total = props.entries.len();
    let selected = props.selected_index.min(total.saturating_sub(1));
    let scroll_offset = if selected >= MAX_VISIBLE_ROWS {
        selected + 1 - MAX_VISIBLE_ROWS
    } else {
        0
    };
    let visible_end = (scroll_offset + MAX_VISIBLE_ROWS).min(total);

    let title = if total > MAX_VISIBLE_ROWS {
        format!(
            " Commands ({}-{} of {})  ↑↓ navigate · Tab complete · Esc dismiss ",
            scroll_offset + 1,
            visible_end,
            total
        )
    } else {
        format!(" Commands ({total})  ↑↓ navigate · Tab complete · Esc dismiss ")
    };

    if props.entries.is_empty() {
        let line = Line::from(vec![Span::styled(
            "  No matching commands",
            StyleToken::new().fg(ThemeToken::TextDisabled),
        )]);
        return UiNode::vertical(vec![UiNode::text(vec![line])], vec![])
            .with_border(BorderStyle::Plain, Some(ThemeToken::Border))
            .with_title(title);
    }

    let mut lines: Vec<Line> = Vec::with_capacity(MAX_VISIBLE_ROWS);
    for (offset, entry) in props.entries[scroll_offset..visible_end].iter().enumerate() {
        let absolute_index = scroll_offset + offset;
        let is_selected = absolute_index == selected;

        let mut name_part = format!("/{}", entry.name());
        if let Some(hint) = entry.arg_hint() {
            name_part.push(' ');
            name_part.push_str(hint);
        }

        let name_style = if is_selected {
            StyleToken::new()
                .fg(ThemeToken::TextHighlight)
                .bg(ThemeToken::TextDisabled)
                .bold()
        } else {
            StyleToken::new().fg(ThemeToken::Info).bold()
        };
        let desc_style = if is_selected {
            StyleToken::new()
                .fg(ThemeToken::TextPrimary)
                .bg(ThemeToken::TextDisabled)
        } else {
            StyleToken::new().fg(ThemeToken::TextSecondary)
        };

        let padded_name = format!(" {name_part:<22}");
        lines.push(Line::from(vec![
            Span::styled(padded_name, name_style),
            Span::styled(format!(" {}", entry.description()), desc_style),
        ]));
    }

    UiNode::vertical(vec![UiNode::text(lines)], vec![])
        .with_border(BorderStyle::Plain, Some(ThemeToken::Border))
        .with_title(title)
}
