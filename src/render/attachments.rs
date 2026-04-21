//! Pending-attachment bar shown between chat and input when the user
//! has pasted images queued for their next prompt.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::domain::State;

pub fn render_attachments(state: &State, frame: &mut Frame, area: Rect) {
    if state.ui.attachments.is_empty() {
        return;
    }
    let spans: Vec<Span<'_>> = std::iter::once(Span::styled(
        "attached: ",
        Style::default().fg(Color::DarkGray),
    ))
    .chain(state.ui.attachments.iter().map(|a| {
        Span::styled(
            format!("[img:{} ({} bytes)] ", a.id, a.size_bytes),
            Style::default().fg(Color::Cyan),
        )
    }))
    .collect();
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}
