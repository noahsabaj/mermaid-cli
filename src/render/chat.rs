//! Chat pane — scrollable message history.
//!
//! Pure — no mutation of state, no I/O. Takes `&State` and paints
//! what the user sees. Rich rendering (code blocks, diffs, markdown)
//! is deferred to C8's full widget port; this ships the minimum
//! needed so the new main loop can be wired up and validated end-
//! to-end.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::domain::{GenPhase, State, TurnState};
use crate::models::MessageRole;

pub fn render_chat(state: &State, frame: &mut Frame, area: Rect) {
    let mut lines: Vec<Line<'_>> = Vec::new();

    for msg in state.session.messages() {
        let (prefix, style) = match msg.role {
            MessageRole::User => (
                "you> ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            MessageRole::Assistant => (
                "mermaid> ",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            MessageRole::System => ("system> ", Style::default().fg(Color::DarkGray)),
            MessageRole::Tool => ("tool> ", Style::default().fg(Color::Yellow)),
        };
        lines.push(Line::from(vec![
            Span::styled(prefix, style),
            Span::raw(truncate(&msg.content, 2_000)),
        ]));
        for action in &msg.actions {
            let (color, marker) = match &action.result {
                crate::agents::ActionResult::Success { .. } => (Color::Green, "✓"),
                crate::agents::ActionResult::Error { .. } => (Color::Red, "✗"),
            };
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(marker, Style::default().fg(color)),
                Span::raw(" "),
                Span::styled(
                    format!("{} {}", action.action_type, truncate(&action.target, 60)),
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
        }
    }

    // Append in-flight content from the current turn (not yet committed
    // to the conversation history).
    if let TurnState::Generating {
        partial_reasoning,
        partial_text,
        phase,
        ..
    } = &state.turn
    {
        if !partial_reasoning.is_empty() {
            lines.push(Line::from(Span::styled(
                format!("  thinking: {}", truncate(partial_reasoning, 500)),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            )));
        }
        if !partial_text.is_empty() {
            lines.push(Line::from(vec![
                Span::styled(
                    "mermaid> ",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(truncate(partial_text, 2_000)),
            ]));
        }
        // Phase hint if nothing has streamed yet.
        if partial_text.is_empty() && partial_reasoning.is_empty() {
            let hint = match phase {
                GenPhase::Sending => "sending...",
                GenPhase::Thinking => "thinking...",
                GenPhase::Streaming => "streaming...",
            };
            lines.push(Line::from(Span::styled(
                hint.to_string(),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            )));
        }
    }

    let title = format!("Chat — {}", state.session.model_id);
    let block = Block::default().borders(Borders::ALL).title(title);
    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((state.ui.chat_scroll as u16, 0));
    frame.render_widget(paragraph, area);
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let cut = s.floor_char_boundary(max);
        format!("{}…", &s[..cut])
    }
}
