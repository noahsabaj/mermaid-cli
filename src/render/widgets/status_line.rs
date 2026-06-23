use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};
use std::collections::VecDeque;

use super::GenerationStatus;
use crate::render::theme::Theme;

/// Props for StatusLineWidget (stateless widget showing generation progress)
pub struct StatusLineWidget<'a> {
    pub status: GenerationStatus,
    pub elapsed_secs: u64,
    pub tokens_received: usize,
    /// Whether tokens_received is an estimate (show ~ prefix)
    pub tokens_estimated: bool,
    pub theme: &'a Theme,
    /// Queued messages waiting to be processed
    pub queued_messages: &'a VecDeque<String>,
    /// The in-flight tool's label while running tools (e.g. `Bash npm run dev`),
    /// so the status line names what it's waiting on. `None` for other phases.
    pub active_tool: Option<String>,
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

        // While running tools, append the in-flight tool so the user can see
        // what's executing (a long `npm run dev` etc. no longer looks opaque).
        let status_text = match (&self.status, &self.active_tool) {
            (GenerationStatus::RunningTools, Some(tool)) => {
                format!("{}: {}", self.status.display_text(), tool)
            },
            _ => self.status.display_text().to_string(),
        };

        let info_color = self.theme.colors.info.to_color();

        // Determine arrow direction based on state
        let (arrow, flow_direction) = match self.status {
            GenerationStatus::Sending | GenerationStatus::Thinking => ("↑ ", "upstream"),
            GenerationStatus::Streaming => ("↓ ", "downstream"),
            GenerationStatus::RunningTools => ("• ", "tools"),
            GenerationStatus::Compacting => ("• ", "compaction"),
            GenerationStatus::Cancelling => ("• ", "cleanup"),
            GenerationStatus::Idle => ("", ""),
        };

        // While tools run, advertise Ctrl+B (send a running command to the
        // background). Harmless no-op for non-detachable tools.
        let bg_hint = if self.status == GenerationStatus::RunningTools {
            " • ctrl+b to background"
        } else {
            ""
        };

        let spans = vec![
            // Arrow indicator showing message direction (cyan)
            Span::styled(arrow, Style::new().fg(info_color)),
            // Status text with ellipsis (cyan)
            Span::styled(format!("{}... ", status_text), Style::new().fg(info_color)),
            // Metadata in parentheses (dimmed)
            // Show ~ prefix when tokens are estimated (during streaming)
            Span::styled(
                format!(
                    "(esc to interrupt{bg_hint} • {}s • {} {}{} tokens)",
                    self.elapsed_secs,
                    if flow_direction == "downstream" {
                        "↓"
                    } else if flow_direction == "tools" {
                        "tools"
                    } else if flow_direction == "compaction" {
                        "compact"
                    } else if flow_direction == "cleanup" {
                        "cleanup"
                    } else {
                        "↑"
                    },
                    if self.tokens_estimated { "~" } else { "" },
                    self.tokens_received
                ),
                Style::new()
                    .fg(self.theme.colors.text_secondary.to_color())
                    .dim(),
            ),
        ];

        let mut lines = vec![Line::from(spans)];

        // Show all queued messages below the status line with highlight
        let max_len = area.width.saturating_sub(4) as usize;
        for queued in self.queued_messages.iter() {
            // Truncate long messages to fit in the area
            let display_msg = if queued.len() > max_len {
                let end = queued.floor_char_boundary(max_len.saturating_sub(3));
                format!("> {}...", &queued[..end])
            } else {
                format!("> {}", queued)
            };

            lines.push(Line::from(vec![Span::styled(
                display_msg,
                Style::new()
                    .fg(self.theme.colors.text_primary.to_color())
                    .bg(ratatui::style::Color::Rgb(60, 60, 80)), // Subtle purple highlight
            )]));
        }

        let paragraph = Paragraph::new(lines);

        paragraph.render(area, buf);
    }
}
