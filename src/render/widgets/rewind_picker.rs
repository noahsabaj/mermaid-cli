//! Double-Esc rewind picker — renders the bottom zone when
//! `UiMode::RewindPicker` is active.
//!
//! Same visual shape as the conversation-list picker: a bordered pane with
//! an arrow-selectable list, one row per earlier USER message (newest
//! first). Selecting a row forks the session at that message.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};

use super::truncate_to_cells;
use crate::domain::RewindCandidate;
use crate::render::theme::Theme;

pub struct RewindPickerWidget<'a> {
    pub theme: &'a Theme,
    pub candidates: &'a [RewindCandidate],
    pub cursor: usize,
}

impl<'a> Widget for RewindPickerWidget<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let title = "Rewind — fork at an earlier message · ↑↓ navigate · Enter fork · Esc cancel";
        let block = Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(Style::default().fg(self.theme.colors.border.to_color()));

        // Reserve room for borders; show up to `visible` rows, keeping the
        // cursor in view (same window math as the conversation list).
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
            .map(|(i, candidate)| {
                let highlighted = i == self.cursor;
                let prefix = if highlighted { " > " } else { "   " };
                let row_style = if highlighted {
                    Style::default()
                        .bg(self.theme.colors.text_disabled.to_color())
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                let excerpt = truncate_to_cells(&candidate.excerpt, 64);
                // 1-based recency label: #1 = the newest user message.
                let meta = format!("  (#{} back)", i + 1);
                Line::from(vec![
                    Span::raw(prefix),
                    Span::styled(excerpt, row_style.fg(Color::White)),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn candidates(excerpts: &[&str]) -> Vec<RewindCandidate> {
        excerpts
            .iter()
            .enumerate()
            .map(|(i, e)| RewindCandidate {
                message_index: i,
                excerpt: e.to_string(),
            })
            .collect()
    }

    fn render_text(widget: RewindPickerWidget<'_>, width: u16, height: u16) -> String {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        widget.render(area, &mut buf);
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn renders_excerpts_with_recency_labels() {
        let theme = Theme::dark();
        let list = candidates(&["fix the resolver", "add tests"]);
        let text = render_text(
            RewindPickerWidget {
                theme: &theme,
                candidates: &list,
                cursor: 0,
            },
            90,
            6,
        );
        assert!(text.contains("Rewind"), "{text}");
        assert!(text.contains("fix the resolver"), "{text}");
        assert!(text.contains("(#2 back)"), "{text}");
    }

    #[test]
    fn out_of_range_cursor_does_not_panic() {
        let theme = Theme::dark();
        let list = candidates(&["only one"]);
        let text = render_text(
            RewindPickerWidget {
                theme: &theme,
                candidates: &list,
                cursor: 42,
            },
            60,
            5,
        );
        // The cursor row scrolled past the end: the list may render empty,
        // but the widget must not panic and the frame stays intact.
        assert!(text.contains("Rewind"), "{text}");
    }
}
