use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Style, Stylize},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};

use crate::tui::app::GenerationStatus;
use crate::tui::theme::Theme;

/// Props for StatusLineWidget (stateless widget showing generation progress)
pub struct StatusLineWidget<'a> {
    pub status: GenerationStatus,
    pub elapsed_secs: u64,
    pub tokens_received: usize,
    pub theme: &'a Theme,
}

impl<'a> Widget for StatusLineWidget<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // Don't render if area is too small
        if area.height == 0 || area.width < 10 {
            return;
        }

        // Only render if status is not Idle
        if self.status == GenerationStatus::Idle {
            return;
        }

        // Build the status line
        let status_text = self.status.display_text();
        let info_color = self.theme.colors.info.to_color();

        let spans = vec![
            // Activity indicator (cyan bullet)
            Span::styled(
                "• ",
                Style::new().fg(info_color).bold(),
            ),
            // Status text (cyan)
            Span::styled(
                format!("{} ", status_text),
                Style::new().fg(info_color),
            ),
            // Ellipsis/animation indicator
            Span::styled(
                "▮ ",
                Style::new().fg(info_color).dim(),
            ),
            // Metadata in parentheses (dimmed)
            Span::styled(
                format!("(esc to interrupt • {}s • ↓ {} tokens)", self.elapsed_secs, self.tokens_received),
                Style::new()
                    .fg(self.theme.colors.text_secondary.to_color())
                    .dim(),
            ),
        ];

        let line = Line::from(spans);
        let paragraph = Paragraph::new(line)
            .style(Style::new().bg(self.theme.colors.background.to_color()));

        paragraph.render(area, buf);
    }
}
