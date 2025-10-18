use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Paragraph, Widget},
};
use ratatui_macros::{line, span};

use crate::tui::mode::OperationMode;
use crate::tui::theme::Theme;

/// Props for StatusWidget (stateless widget)
pub struct StatusWidget<'a> {
    pub operation_mode: OperationMode,
    pub status_message: Option<&'a str>,
    pub confirmation_pending: bool,
    pub is_generating: bool,
    pub theme: &'a Theme,
}

impl<'a> Widget for StatusWidget<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let mut lines = Vec::new();

        // Line 1: Mode badge + Status message + inline warnings
        let status_text = if self.confirmation_pending {
            "Action pending".to_string()
        } else if let Some(status) = self.status_message {
            status.to_string()
        } else if self.is_generating {
            "Generating...".to_string()
        } else {
            "Ready".to_string()
        };

        let mut line1_spans = vec![
            Span::styled(
                format!(" {} ", self.operation_mode.display_name()),
                Style::new()
                    .bg(self.operation_mode.color())
                    .fg(self.theme.colors.background.to_color())
                    .bold(),
            ),
            Span::raw(" "),
            Span::styled(
                status_text,
                Style::new().fg(self.theme.colors.text_primary.to_color()),
            ),
        ];

        // Add inline warnings/prompts to Line 1
        if self.confirmation_pending {
            line1_spans.push(Span::raw(" • "));
            line1_spans.push(Span::styled(
                "Alt+Y: approve, Alt+N: skip, Alt+P: preview",
                Style::new().fg(self.theme.colors.warning.to_color()).bold(),
            ));
        } else {
            let warning_level = self.operation_mode.warning_level();
            if let Some(warning) = warning_level.message() {
                let warning_text = warning.to_string();
                let warning_color = warning_level.color();
                line1_spans.push(Span::raw(" • "));
                line1_spans.push(Span::styled(
                    warning_text,
                    Style::new().fg(warning_color).bold(),
                ));
            }
        }

        lines.push(Line::from(line1_spans));

        // Line 2: Keyboard shortcuts (compact, always visible)
        lines.push(line![
            span!(Style::new().fg(self.theme.colors.text_disabled.to_color()).dim(); " Shift+Tab "),
            span!(self.theme.colors.text_secondary.to_color(); "modes"),
            span!(self.theme.colors.text_disabled.to_color(); " • "),
            span!(Style::new().fg(self.theme.colors.text_disabled.to_color()).dim(); "Ctrl+S "),
            span!(self.theme.colors.text_secondary.to_color(); "sidebar"),
            span!(self.theme.colors.text_disabled.to_color(); " • "),
            span!(Style::new().fg(self.theme.colors.text_disabled.to_color()).dim(); "Ctrl+D "),
            span!(self.theme.colors.text_secondary.to_color(); "diagnostics"),
            span!(self.theme.colors.text_disabled.to_color(); " • "),
            span!(Style::new().fg(self.theme.colors.text_disabled.to_color()).dim(); ":help"),
            span!(self.theme.colors.text_secondary.to_color(); " for commands"),
        ]);

        let status_bar = Paragraph::new(lines)
            .style(Style::new().bg(self.theme.colors.background.to_color()))
            .block(Block::default());

        status_bar.render(area, buf);
    }
}
