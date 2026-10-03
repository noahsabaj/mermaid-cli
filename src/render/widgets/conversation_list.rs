//! `/load` picker — renders the bottom zone when
//! `UiMode::ConversationList` is active.
//!
//! Same visual shape as the slash palette (a bordered pane with an
//! arrow-selectable list) but with richer per-row content: title,
//! message count, updated-at timestamp.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};

use super::truncate_to_cells;
use crate::render::theme::Theme;
use mermaid_domain::ConversationSummary;

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
                let colors = &self.theme.colors;
                // The highlighted row lies on the prompt band, bold, with its
                // meta in the title's ink: the meta's usual text_disabled is
                // too faint to read on a band.
                let (row_style, meta_color) = if highlighted {
                    (
                        Style::default()
                            .bg(colors.user_message_background.to_color())
                            .add_modifier(Modifier::BOLD),
                        colors.text_primary.to_color(),
                    )
                } else {
                    (Style::default(), colors.text_disabled.to_color())
                };
                let title = truncate_to_cells(&summary.title, 48);
                let meta = format!(
                    "  ({} msg · {})",
                    summary.message_count,
                    short_timestamp(&summary.updated_at)
                );
                Line::from(vec![
                    Span::raw(prefix),
                    Span::styled(title, row_style.fg(colors.text_primary.to_color())),
                    Span::styled(meta, row_style.fg(meta_color)),
                ])
            })
            .collect();

        Paragraph::new(rows).block(block).render(area, buf);
    }
}

/// `2026-04-21T14:30:12-04:00` → `2026-04-21 14:30`. If parsing fails
/// for any reason, returns the original string.
fn short_timestamp(rfc3339: &str) -> String {
    // Extract the `YYYY-MM-DDTHH:MM` portion (16 ASCII bytes) and swap the
    // 'T' for a space. Clamp to a char boundary so a malformed value with a
    // multi-byte sequence straddling byte 16 can't panic the slice (#102).
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

    #[test]
    fn short_timestamp_passes_through_short_input() {
        assert_eq!(short_timestamp("2026"), "2026");
        assert_eq!(short_timestamp(""), "");
    }

    #[test]
    fn highlighted_row_meta_is_readable_on_its_band() {
        // On the highlighted row the meta takes the title's ink over the
        // band; in the band's own colour it would not be seen at all.
        let candidates = vec![ConversationSummary {
            id: "a".to_string(),
            title: "Fix the resolver panic".to_string(),
            message_count: 14,
            updated_at: "2026-01-01T12:34:00-04:00".to_string(),
        }];
        for make_theme in [Theme::dark, Theme::light] {
            let theme = make_theme();
            let area = Rect::new(0, 0, 80, 4);
            let mut buf = Buffer::empty(area);
            ConversationListWidget {
                theme: &theme,
                candidates: &candidates,
                cursor: 0,
            }
            .render(area, &mut buf);
            let x = (0..area.width)
                .find(|&x| buf[(x, 1)].symbol() == "(")
                .expect("the meta is drawn on the first row");
            let cell = &buf[(x, 1)];
            let colors = &theme.colors;
            assert_eq!(
                cell.bg,
                colors.user_message_background.to_color(),
                "{}",
                theme.name
            );
            assert_eq!(cell.fg, colors.text_primary.to_color(), "{}", theme.name);
            assert_ne!(
                cell.fg, cell.bg,
                "{}: meta drawn in its band's colour",
                theme.name
            );
        }
    }

    #[test]
    fn short_timestamp_does_not_panic_on_multibyte_boundary() {
        // "2026-04-21T14:3" is 15 bytes; then "好" (3 bytes) straddles byte 16.
        // floor_char_boundary(16) backs up to byte 15 instead of panicking (#102).
        assert_eq!(short_timestamp("2026-04-21T14:3好0:12"), "2026-04-21 14:3");
    }
}
