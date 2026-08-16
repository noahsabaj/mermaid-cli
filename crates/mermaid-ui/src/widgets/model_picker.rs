use crate::node::{BorderStyle, Line, Span, UiNode};
use crate::theme::{StyleToken, Theme, ThemeToken};
use crate::wrap::truncate_to_cells;
use mermaid_domain::ModelChoice;
use unicode_width::UnicodeWidthStr;

pub const MODEL_PICKER_VISIBLE_ROWS: usize = 10;
pub const MODEL_PICKER_HEIGHT: u16 = MODEL_PICKER_VISIBLE_ROWS as u16 + 3;

#[derive(Debug, Clone)]
pub struct ModelPickerProps<'a> {
    pub theme: &'a Theme,
    pub matches: &'a [&'a ModelChoice],
    pub query: &'a str,
    pub cursor: usize,
    pub loading: bool,
    pub current: &'a str,
    pub width: usize,
    pub height: usize,
}

#[must_use]
pub fn build_model_picker_view(props: ModelPickerProps<'_>) -> UiNode {
    let dim = StyleToken::new().fg(ThemeToken::TextDisabled);
    let inner_height = props.height.saturating_sub(2);
    let visible = inner_height
        .saturating_sub(1)
        .min(MODEL_PICKER_VISIBLE_ROWS);
    let cursor = props.cursor.min(props.matches.len().saturating_sub(1));
    let mut lines: Vec<Line> = Vec::new();

    if props.matches.is_empty() {
        lines.push(Line::from(Span::styled(
            if props.loading {
                "  searching for available models…".to_string()
            } else if props.query.is_empty() {
                "  No models found. Pull one with `ollama pull`, or set a provider API key."
                    .to_string()
            } else {
                format!("  Nothing matches {:?}.", props.query)
            },
            dim,
        )));
    } else {
        let start = window_start(props.matches, cursor, visible);
        let mut last_group: Option<&str> = None;
        for (i, choice) in props.matches.iter().enumerate().skip(start) {
            if lines.len() >= visible {
                break;
            }
            if last_group != Some(choice.group.as_str()) {
                last_group = Some(choice.group.as_str());
                if lines.len() + 2 > visible && !lines.is_empty() {
                    break;
                }
                lines.push(Line::from(Span::styled(
                    format!(" {}", choice.group),
                    StyleToken::new().fg(ThemeToken::Header).bold(),
                )));
            }
            lines.push(format_model_row(
                choice,
                i == cursor,
                props.current,
                props.width,
            ));
        }
    }

    let status = if props.query.is_empty() {
        let shown = props.matches.len();
        if props.loading {
            " filter: (type to narrow) · still searching…".to_string()
        } else {
            format!(" filter: (type to narrow) · {shown} models")
        }
    } else {
        format!(
            " filter: {} · {} match{}",
            props.query,
            props.matches.len(),
            if props.matches.len() == 1 { "" } else { "es" }
        )
    };

    let footer = match props.matches.get(cursor) {
        Some(choice) => {
            let both = format!("{status} · {}", choice.id);
            if both.width() <= props.width {
                both
            } else {
                truncate_to_cells(&format!(" {}", choice.id), props.width)
            }
        },
        None => status,
    };
    lines.push(Line::from(Span::styled(footer, dim)));

    UiNode::vertical(vec![UiNode::text(lines)], vec![])
        .with_border(BorderStyle::Plain, Some(ThemeToken::Border))
        .with_title(
            "Select model — ↑↓ navigate · Enter switch · type to filter · Esc cancel".to_string(),
        )
}

fn window_start(matches: &[&ModelChoice], cursor: usize, visible: usize) -> usize {
    let mut start = cursor;
    let mut cost = 2usize;
    while start > 0 {
        let extra = if matches[start - 1].group == matches[start].group {
            1
        } else {
            2
        };
        if cost + extra > visible {
            break;
        }
        cost += extra;
        start -= 1;
    }
    start
}

fn display_id(choice: &ModelChoice) -> &str {
    let Some((prefix, rest)) = choice.id.split_once('/') else {
        return &choice.id;
    };
    if rest.is_empty() {
        return &choice.id;
    }
    let group = choice.group.to_ascii_lowercase();
    let prefix = prefix.to_ascii_lowercase();
    if group == prefix || group.contains(&format!("({prefix})")) {
        rest
    } else {
        &choice.id
    }
}

fn format_model_row(choice: &ModelChoice, highlighted: bool, current: &str, width: usize) -> Line {
    let prefix = if highlighted { " > " } else { "   " };
    let id_style = if highlighted {
        StyleToken::new().fg(ThemeToken::Brand).bold()
    } else {
        StyleToken::new().fg(ThemeToken::TextPrimary)
    };
    let current_mark = if choice.id == current {
        " (current)"
    } else {
        ""
    };
    let pull_mark = if choice.ready { "" } else { " (not pulled)" };
    let reserved = prefix.width() + current_mark.width() + pull_mark.width();
    let id = truncate_to_cells(display_id(choice), width.saturating_sub(reserved));

    let mut spans = vec![
        Span::styled(prefix, StyleToken::new().fg(ThemeToken::Brand)),
        Span::styled(id, id_style),
    ];
    if !current_mark.is_empty() {
        spans.push(Span::styled(
            current_mark,
            StyleToken::new().fg(ThemeToken::Success),
        ));
    }
    if !pull_mark.is_empty() {
        spans.push(Span::styled(
            pull_mark,
            StyleToken::new().fg(ThemeToken::Warning),
        ));
    }
    if !choice.detail.is_empty() {
        let used: usize = spans.iter().map(Span::width).sum();
        let detail_width = choice.detail.width();
        if used + detail_width + 2 <= width {
            spans.push(Span::raw(" ".repeat(width - used - detail_width - 1)));
            spans.push(Span::styled(
                choice.detail.clone(),
                StyleToken::new().fg(ThemeToken::TextDisabled),
            ));
        }
    }
    Line::from(spans)
}
