//! Attachment indicator widget
//!
//! Renders [Image #N] indicators above the input box when images are attached.
//! Shows help text for navigation: "(up to select)" normally, or navigation hints when focused.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Widget,
};

use crate::tui::state::attachment::ImageAttachment;
use crate::tui::theme::Theme;

/// Widget that displays attached image indicators
pub struct AttachmentWidget<'a> {
    pub attachments: &'a [ImageAttachment],
    pub theme: &'a Theme,
    /// Whether the attachment area is focused (for selection highlight)
    pub focused: bool,
    /// Which attachment is selected (only relevant when focused)
    pub selected: usize,
}

impl<'a> Widget for AttachmentWidget<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if self.attachments.is_empty() || area.height == 0 {
            return;
        }

        let hint_style = Style::new().fg(self.theme.colors.text_secondary.to_color());

        // Render all attachments on a single line with help text
        let mut spans = vec![Span::raw("  ")];

        for (i, attachment) in self.attachments.iter().enumerate() {
            let is_selected = self.focused && i == self.selected;
            let size_display = format_size(attachment.size_bytes);

            let label_style = if is_selected {
                Style::new()
                    .fg(self.theme.colors.background.to_color())
                    .bg(self.theme.colors.info.to_color())
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::new()
                    .fg(self.theme.colors.info.to_color())
                    .add_modifier(Modifier::BOLD)
            };

            spans.push(Span::styled(format!("[Image #{}]", i + 1), label_style));
            spans.push(Span::styled(
                format!(
                    " ({}, {})  ",
                    attachment.format.to_uppercase(),
                    size_display
                ),
                hint_style,
            ));
        }

        // Add help text after the images
        if self.focused {
            spans.push(Span::styled(
                "→ to next · ← to prev · Delete to remove · Esc to cancel",
                hint_style,
            ));
        } else {
            spans.push(Span::styled("(↑ to select)", hint_style));
        }

        let line = Line::from(spans);
        buf.set_line(area.x, area.y, &line, area.width);
    }
}

fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{}B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.0}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}
