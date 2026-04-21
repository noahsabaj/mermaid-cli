//! One-line status bar under the input box.
//!
//! Lives for the duration of a `StatusLine`; `Msg::StatusDismiss`
//! clears it. Kind → color mapping is the only styling that lives
//! here.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::Span;
use ratatui::widgets::Paragraph;

use crate::domain::{State, StatusKind};

pub fn render_status(state: &State, frame: &mut Frame, area: Rect) {
    let Some(status) = &state.status else {
        return;
    };
    let color = match status.kind {
        StatusKind::Info => Color::Cyan,
        StatusKind::Warn => Color::Yellow,
        StatusKind::Error => Color::Red,
        StatusKind::Persistent => Color::DarkGray,
    };
    let span = Span::styled(status.text.as_str(), Style::default().fg(color));
    let paragraph = Paragraph::new(span);
    frame.render_widget(paragraph, area);
}
