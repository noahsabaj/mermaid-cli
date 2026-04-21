//! `/load` picker — renders the bottom zone when
//! `UiMode::ConversationList` is active.
//!
//! Same visual shape as the slash palette (a bordered pane with an
//! arrow-selectable list) but with richer per-row content: title,
//! message count, updated-at timestamp.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};

use crate::domain::ConversationSummary;
use crate::render::theme::Theme;

pub struct ConversationListWidget<'a> {
    pub theme: &'a Theme,
    pub candidates: &'a [ConversationSummary],
    pub cursor: usize,
}

impl<'a> Widget for ConversationListWidget<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let title = if self.candidates.is_empty() {
            "Load conversation — (none found)"
        } else {
            "Load conversation — ↑↓ navigate · Enter select · Esc cancel"
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(Style::default().fg(self.theme.colors.border.to_color()));

        // Reserve room for borders; show up to `visible` rows.
        let inner_height = area.height.saturating_sub(2) as usize;
        let visible = inner_height.min(10);
        let start = if self.cursor >= visible {
            self.cursor + 1 - visible
        } else {
            0
        };

        let rows: Vec<Line<'_>> = self
            .candidates
            .iter()
            .enumerate()
            .skip(start)
            .take(visible)
            .map(|(i, summary)| {
                let highlighted = i == self.cursor;
                let prefix = if highlighted { " > " } else { "   " };
                let row_style = if highlighted {
                    Style::default()
                        .bg(self.theme.colors.text_disabled.to_color())
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                let title = truncate(&summary.title, 48);
                let meta = format!(
                    "  ({} msg · {})",
                    summary.message_count,
                    short_timestamp(&summary.updated_at)
                );
                Line::from(vec![
                    Span::raw(prefix),
                    Span::styled(title, row_style.fg(Color::White)),
                    Span::styled(
                        meta,
                        row_style.fg(self.theme.colors.text_disabled.to_color()),
                    ),
                ])
            })
            .collect();

        Paragraph::new(rows).block(block).render(area, buf);
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut = s.floor_char_boundary(max);
        format!("{}…", &s[..cut])
    }
}

/// `2026-04-21T14:30:12-04:00` → `2026-04-21 14:30`. If parsing fails
/// for any reason, returns the original string.
fn short_timestamp(rfc3339: &str) -> String {
    // Extract up to 16 chars of the RFC3339 date/time portion.
    // `YYYY-MM-DDTHH:MM` → swap the 'T' for a space.
    if rfc3339.len() >= 16 {
        let mut s = rfc3339[..16].to_string();
        if let Some(t_pos) = s.find('T') {
            s.replace_range(t_pos..t_pos + 1, " ");
        }
        s
    } else {
        rfc3339.to_string()
    }
}
