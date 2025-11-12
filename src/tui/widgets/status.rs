use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Style, Stylize},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};

use crate::tui::mode::OperationMode;
use crate::tui::theme::Theme;

/// Props for StatusWidget (stateless widget)
pub struct StatusWidget<'a> {
    pub operation_mode: OperationMode,
    pub confirmation_pending: bool,
    pub theme: &'a Theme,
    pub working_dir: &'a str,
    pub cumulative_tokens: usize,
}

impl<'a> Widget for StatusWidget<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // Get hostname and username for directory display
        let hostname = std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("HOST"))
            .unwrap_or_else(|_| "localhost".to_string());
        let username = std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_else(|_| "user".to_string());

        // Line 1: username@hostname:/path (left) | tokens (right, fixed position)
        let directory_text = format!("{}@{}:{}", username, hostname, self.working_dir);
        let token_text = format!("{} tokens", self.cumulative_tokens);

        // Calculate padding to push tokens to right edge
        let available_width = area.width as usize;
        let padding_width = if available_width > directory_text.len() + token_text.len() + 1 {
            available_width - directory_text.len() - token_text.len()
        } else {
            1
        };

        let line1_spans = vec![
            // Directory (fixed to left)
            Span::styled(
                format!("{}@{}", username, hostname),
                Style::new().fg(ratatui::style::Color::Green).bold(),
            ),
            Span::styled(":", Style::new().fg(self.theme.colors.text_primary.to_color())),
            Span::styled(
                self.working_dir,
                Style::new().fg(ratatui::style::Color::Cyan),
            ),
            // Padding
            Span::raw(" ".repeat(padding_width)),
            // Token count (fixed to right)
            Span::styled(
                token_text,
                Style::new().fg(self.theme.colors.text_disabled.to_color()),
            ),
        ];

        // Line 2: operation mode (colored text only, no background badge)
        let mode_text = match self.operation_mode {
            OperationMode::Normal => {
                if self.confirmation_pending {
                    "action pending (alt+y/n/p)"
                } else {
                    ""  // No text in normal mode when not pending
                }
            },
            OperationMode::AcceptEdits => "accept edits on (shift+tab to cycle)",
            OperationMode::PlanMode => "plan mode on (shift+tab to cycle)",
            OperationMode::BypassAll => "bypass permissions on (shift+tab to cycle)",
        };

        let line2_spans = if !mode_text.is_empty() {
            vec![
                Span::styled(
                    mode_text,
                    Style::new().fg(self.operation_mode.color()),
                ),
            ]
        } else {
            vec![Span::raw("")]  // Empty line when no mode text
        };

        let line1 = Line::from(line1_spans);
        let line2 = Line::from(line2_spans);
        let status_bar = Paragraph::new(vec![line1, line2]);

        status_bar.render(area, buf);
    }
}
