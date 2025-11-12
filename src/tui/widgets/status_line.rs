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
    pub custom_status: Option<&'a String>,
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
        // Use custom status if provided, otherwise use the default status text
        let status_text = if let Some(custom) = self.custom_status {
            custom.as_str()
        } else {
            self.status.display_text()
        };

        let info_color = self.theme.colors.info.to_color();

        // Determine arrow direction based on state
        let (arrow, flow_direction) = match self.status {
            GenerationStatus::Sending |
            GenerationStatus::Initializing |
            GenerationStatus::Thinking => ("↑ ", "upstream"),
            GenerationStatus::Streaming => ("↓ ", "downstream"),
            GenerationStatus::Idle => ("", ""),
        };

        let spans = vec![
            // Arrow indicator showing message direction (cyan)
            Span::styled(
                arrow,
                Style::new().fg(info_color),
            ),
            // Status text with ellipsis (cyan)
            Span::styled(
                format!("{}... ", status_text),
                Style::new().fg(info_color),
            ),
            // Metadata in parentheses (dimmed)
            Span::styled(
                format!("(esc to interrupt • {}s • {} {} tokens)",
                    self.elapsed_secs,
                    if flow_direction == "downstream" { "↓" } else { "↑" },
                    self.tokens_received),
                Style::new()
                    .fg(self.theme.colors.text_secondary.to_color())
                    .dim(),
            ),
        ];

        let line = Line::from(spans);
        let paragraph = Paragraph::new(line);

        paragraph.render(area, buf);
    }
}
