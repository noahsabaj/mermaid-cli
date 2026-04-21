use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};
use unicode_width::UnicodeWidthStr;

use crate::models::ReasoningLevel;
use crate::render::theme::Theme;

/// Props for StatusWidget (stateless widget)
pub struct StatusWidget<'a> {
    pub theme: &'a Theme,
    pub working_dir: &'a str,
    pub cumulative_tokens: usize,
    pub model_name: &'a str,
    /// Effective reasoning depth — what the API actually saw after
    /// `nearest_effort` snapping against the model's capabilities. Always
    /// rendered on line 2 left.
    pub reasoning_level: ReasoningLevel,
    /// User-requested level when it differs from `reasoning_level` (the
    /// snap case). `Some(requested)` shows `reasoning: high (max
    /// requested)`; `None` shows just `reasoning: high`.
    pub requested_level: Option<ReasoningLevel>,
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

        // Calculate padding to push tokens to right edge. Use display-cell
        // widths so CJK / emoji chars in working_dir or hostname don't
        // misalign the right-anchored token count.
        let available_width = area.width as usize;
        let directory_width = directory_text.width();
        let token_width = token_text.width();
        let padding_width = if available_width > directory_width + token_width + 1 {
            available_width - directory_width - token_width
        } else {
            1
        };

        let line1_spans = vec![
            // Directory (fixed to left)
            Span::styled(
                format!("{}@{}", username, hostname),
                Style::new().fg(ratatui::style::Color::Green).bold(),
            ),
            Span::styled(
                ":",
                Style::new().fg(self.theme.colors.text_primary.to_color()),
            ),
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

        // Line 2: "reasoning: <level>" (or "<level> (<requested> requested)"
        // when the user's requested level got snapped to a lower one by
        // the model's capability ceiling) | model name (right).
        let reasoning_text = match self.requested_level {
            Some(requested) => format!(
                "reasoning: {} ({} requested)",
                self.reasoning_level.as_str(),
                requested.as_str()
            ),
            None => format!("reasoning: {}", self.reasoning_level.as_str()),
        };
        let model_display = self.model_name;

        // Calculate padding between reasoning text and model name (display-cell widths).
        let left_content_width = reasoning_text.width();
        let right_content_width = model_display.width();
        let padding_width_line2 = if available_width > left_content_width + right_content_width {
            available_width - left_content_width - right_content_width
        } else {
            1
        };

        let line2_spans = vec![
            // "reasoning: <level>" text (left, gray, always rendered)
            Span::styled(
                reasoning_text,
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
