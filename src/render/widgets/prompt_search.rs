//! Ctrl+R prompt search — renders the bottom zone when
//! `UiMode::PromptSearch` is active.
//!
//! Same visual shape as the rewind picker: a bordered pane, the query in the
//! title, and one row per matching earlier prompt (newest first).

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};

use super::truncate_to_cells;
use crate::render::theme::Theme;

pub struct PromptSearchWidget<'a> {
    pub theme: &'a Theme,
    /// The candidates the query matches, newest first.
    pub matches: &'a [&'a String],
    pub query: &'a str,
    pub cursor: usize,
    /// Saved sessions' prompts are still loading.
    pub loading: bool,
}

impl<'a> Widget for PromptSearchWidget<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let title = format!(
            "Search prompts: {}_ · type to filter · ↑↓ or Ctrl+R move · Enter use · Esc cancel",
            self.query
        );
        let block = Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(Style::default().fg(self.theme.colors.border.to_color()));

        let inner_height = area.height.saturating_sub(2) as usize;
        let visible = inner_height.min(10);
        let start = if self.cursor >= visible {
            self.cursor + 1 - visible
        } else {
            0
        };
        let colors = &self.theme.colors;
        let row_width = area.width.saturating_sub(5) as usize;

        let mut rows: Vec<Line<'_>> = self
            .matches
            .iter()
            .enumerate()
            .skip(start)
            .take(visible)
            .map(|(i, prompt)| {
                let highlighted = i == self.cursor;
                let prefix = if highlighted { " > " } else { "   " };
                let style = if highlighted {
                    Style::default()
                        .bg(colors.user_message_background.to_color())
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                // One row per prompt: a multi-line prompt shows its lines
                // joined, so the row still reads as the prompt it was.
                let flat = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
                Line::from(vec![
                    Span::raw(prefix),
                    Span::styled(
                        truncate_to_cells(&flat, row_width),
                        style.fg(colors.text_primary.to_color()),
                    ),
                ])
            })
            .collect();
        if rows.is_empty() {
            let note = if self.loading {
                "   Loading saved prompts..."
            } else {
                "   No earlier prompt matches"
            };
            rows.push(Line::from(Span::styled(
                note,
                Style::default().fg(colors.text_disabled.to_color()),
            )));
        }

        Paragraph::new(rows).block(block).render(area, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render_text(widget: PromptSearchWidget<'_>, width: u16, height: u16) -> String {
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
    fn renders_query_matches_and_the_empty_states() {
        let theme = Theme::dark();
        let a = "fix the parser\nand its tests".to_string();
        let b = "add a flag".to_string();
        let matches = vec![&a, &b];
        let text = render_text(
            PromptSearchWidget {
                theme: &theme,
                matches: &matches,
                query: "fi",
                cursor: 0,
                loading: false,
            },
            100,
            6,
        );
        assert!(text.contains("Search prompts: fi_"), "{text}");
        assert!(text.contains(" > fix the parser and its tests"), "{text}");
        assert!(text.contains("   add a flag"), "{text}");

        let none: Vec<&String> = Vec::new();
        let loading = render_text(
            PromptSearchWidget {
                theme: &theme,
                matches: &none,
                query: "",
                cursor: 0,
                loading: true,
            },
            100,
            4,
        );
        assert!(loading.contains("Loading saved prompts"), "{loading}");
    }
}
