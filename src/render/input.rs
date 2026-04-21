//! Input box — shows `state.ui.input_buffer` inside a bordered box.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::domain::{State, TurnState};

pub fn render_input(state: &State, frame: &mut Frame, area: Rect) {
    let title = if matches!(state.turn, TurnState::Cancelling { .. }) {
        "cancelling (Ctrl+C)…"
    } else if state.is_busy() {
        "working… press Esc to cancel"
    } else {
        "prompt — Enter submits, Ctrl+C exits"
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(Color::DarkGray));
    let text = if state.ui.input_buffer.is_empty() {
        hint_for(state).to_string()
    } else {
        state.ui.input_buffer.clone()
    };
    let style = if state.ui.input_buffer.is_empty() {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default()
    };
    let paragraph = Paragraph::new(text)
        .block(block)
        .style(style)
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn hint_for(state: &State) -> &'static str {
    match &state.turn {
        TurnState::Idle => "Ask me anything. Type /help for commands.",
        TurnState::Generating { .. } => "Waiting for the model…",
        TurnState::ExecutingTools { .. } => "Running tools…",
        TurnState::RunningSubagents { .. } => "Subagents in flight…",
        TurnState::Cancelling { .. } => "Cancelling…",
    }
}
