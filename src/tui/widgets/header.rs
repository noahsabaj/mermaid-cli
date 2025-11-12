use ratatui::{
    buffer::Buffer,
    layout::{Alignment, Rect},
    style::{Style, Stylize},
    widgets::{Paragraph, Widget},
};
use ratatui_macros::{line, span, text};

use crate::tui::theme::Theme;

/// Props for HeaderWidget (stateless widget)
pub struct HeaderWidget<'a> {
    pub model_name: &'a str,
    pub working_dir: &'a str,
    pub theme: &'a Theme,
}

impl<'a> Widget for HeaderWidget<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let short_path = std::path::Path::new(self.working_dir)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(self.working_dir);

        let header_text = text![line![
            span!(Style::new().bold().fg(self.theme.colors.header.to_color()); "Mermaid"),
            span!(self.theme.colors.text_disabled.to_color(); " • "),
            span!(self.theme.colors.success.to_color(); self.model_name),
            span!(self.theme.colors.text_disabled.to_color(); " • "),
            span!(self.theme.colors.text_secondary.to_color(); short_path),
        ]];

        let header = Paragraph::new(header_text)
            .alignment(Alignment::Center)
            .style(Style::new().bg(self.theme.colors.background.to_color()));

        header.render(area, buf);
    }
}
