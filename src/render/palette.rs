//! Slash-command palette overlay.
//!
//! Shown when `state.ui.mode == UiMode::Palette`. Filters the
//! single COMMAND_REGISTRY by the current `palette_filter` and draws
//! a floating list in the middle of the chat area.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::domain::{State, UiMode};
use crate::tui::slash_commands::filter_by_prefix;

pub fn maybe_render_palette(state: &State, frame: &mut Frame, area: Rect) {
    if state.ui.mode != UiMode::Palette {
        return;
    }
    let commands = filter_by_prefix(&state.ui.palette_filter);
    let visible = commands.iter().take(8).enumerate();

    let lines: Vec<Line<'_>> = visible
        .map(|(i, cmd)| {
            let highlighted = Some(i) == state.ui.palette_cursor;
            let prefix = if highlighted { " > " } else { "   " };
            let style = if highlighted {
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            Line::from(vec![
                Span::raw(prefix),
                Span::styled(format!("/{}", cmd.name), style.fg(Color::Cyan)),
                Span::raw("  "),
                Span::styled(cmd.description, style.fg(Color::DarkGray)),
            ])
        })
        .collect();

    let popup = popup_rect(area, 60, 10);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .title("Commands")
        .border_style(Style::default().fg(Color::Cyan));
    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, popup);
}

fn popup_rect(area: Rect, percent_x: u16, percent_y: u16) -> Rect {
    let vert = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vert[1])[1]
}
