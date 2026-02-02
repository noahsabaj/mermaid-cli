use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};

use crate::tui::theme::Theme;

/// Props for StatusWidget (stateless widget)
pub struct StatusWidget<'a> {
    pub theme: &'a Theme,
    pub working_dir: &'a str,
    pub cumulative_tokens: usize,
    pub model_name: &'a str,
    /// Thinking mode state: Some(true)=ON, Some(false)=OFF, None=unsupported
    pub thinking_enabled: Option<bool>,
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

        // Line 2: "thinking on" (left, only when enabled) | model name (right)
        let thinking_text = if self.thinking_enabled == Some(true) {
            "thinking on"
        } else {
            ""
        };
        let model_display = self.model_name;

        // Calculate padding between thinking text and model name
        let left_content_len = thinking_text.len();
        let right_content_len = model_display.len();
        let padding_width_line2 = if available_width > left_content_len + right_content_len {
            available_width - left_content_len - right_content_len
        } else {
            1
        };

        let line2_spans = vec![
            // "thinking on" text (left, gray, only when enabled)
            Span::styled(
                thinking_text,
                Style::new().fg(self.theme.colors.text_disabled.to_color()),
            ),
            // Padding to right-align model name
            Span::raw(" ".repeat(padding_width_line2)),
            // Model name (right, aligned with tokens above)
            Span::styled(
                model_display,
                Style::new().fg(self.theme.colors.text_disabled.to_color()),
            ),
        ];

        let line1 = Line::from(line1_spans);
        let line2 = Line::from(line2_spans);
        let status_bar = Paragraph::new(vec![line1, line2]);

        status_bar.render(area, buf);
    }
}
