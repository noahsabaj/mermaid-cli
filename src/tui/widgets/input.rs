use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, StatefulWidget, Widget, Wrap},
};

use crate::tui::theme::Theme;

/// State for the input widget
#[derive(Debug, Clone)]
pub struct InputState {
    /// Cursor position in the input string
    pub cursor_position: usize,
}

impl InputState {
    /// Create a new input state
    pub fn new() -> Self {
        Self {
            cursor_position: 0,
        }
    }

    /// Calculate cursor position for wrapped text
    pub fn calculate_cursor_position(input: &str, cursor_pos: usize, inner_width: usize) -> (u16, u16) {
        let cursor_pos = cursor_pos.min(input.len());
        let mut current_line = 0;
        let mut current_col = 0;
        let mut char_count = 0;

        for ch in input.chars() {
            if char_count == cursor_pos {
                break;
            }

            if ch == '\n' || current_col >= inner_width {
                current_line += 1;
                current_col = if ch == '\n' { 0 } else { 1 };
            } else {
                current_col += 1;
            }
            char_count += 1;
        }

        (current_line as u16, current_col as u16)
    }
}

impl Default for InputState {
    fn default() -> Self {
        Self::new()
    }
}

/// Props for InputWidget
pub struct InputWidget<'a> {
    pub input: &'a str,
    pub showing_command_hints: bool,
    pub theme: &'a Theme,
}

impl<'a> StatefulWidget for InputWidget<'a> {
    type State = InputState;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        let (_hints_area, input_area) = if self.showing_command_hints {
            let commands = vec![
                (":quit", "Quit the application"),
                (":q", "Quit (shortcut)"),
                (":clear", "Clear chat history"),
                (":model [name]", "Switch model or show current"),
                (":sidebar", "Toggle file sidebar"),
                (":sb", "Toggle sidebar (shortcut)"),
                (":refresh", "Refresh file context from disk"),
                (":r", "Refresh (shortcut)"),
                (":help", "Show command help"),
                (":h", "Help (shortcut)"),
            ];

            let typed_command = self.input.trim_start_matches(':').to_lowercase();
            let filtered_commands: Vec<_> = if typed_command.is_empty() {
                commands.clone()
            } else {
                commands
                    .into_iter()
                    .filter(|(cmd, _)| {
                        cmd.trim_start_matches(':')
                            .to_lowercase()
                            .starts_with(&typed_command)
                    })
                    .collect()
            };

            let hints_height = (filtered_commands.len() as u16 + 2).min(8);

            if area.height > hints_height {
                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Length(hints_height), Constraint::Min(3)])
                    .split(area);

                if !filtered_commands.is_empty() {
                    let mut hint_lines = vec![Line::from(vec![Span::styled(
                        " Available Commands:",
                        Style::new()
                            .fg(self.theme.colors.info.to_color())
                            .bold(),
                    )])];

                    for (cmd, desc) in filtered_commands.iter().take(6) {
                        hint_lines.push(Line::from(vec![
                            Span::styled(
                                format!("  {:<20}", cmd),
                                Style::new()
                                    .fg(self.theme.colors.text_highlight.to_color())
                                    .bold(),
                            ),
                            Span::styled(
                                *desc,
                                Style::new().fg(self.theme.colors.text_secondary.to_color()),
                            ),
                        ]));
                    }

                    let hints_block = Paragraph::new(hint_lines).block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_style(Style::new().fg(self.theme.colors.border.to_color()))
                            .title(" Commands (up/down to navigate, Enter to execute) "),
                    );

                    hints_block.render(chunks[0], buf);
                }

                (Some(chunks[0]), chunks[1])
            } else {
                (None, area)
            }
        } else {
            (None, area)
        };

        let input_style = Style::new().fg(self.theme.colors.text_primary.to_color());
        let title = if self.showing_command_hints {
            " Enter Command "
        } else {
            " Message (Esc to stop/clear • Type :help for commands) "
        };

        let input = Paragraph::new(self.input)
            .style(input_style)
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::new().fg(if self.showing_command_hints {
                        self.theme.colors.warning.to_color()
                    } else {
                        self.theme.colors.border.to_color()
                    }))
                    .title(title),
            );

        input.render(input_area, buf);

        // Note: Cursor positioning is handled in the main render loop after all widgets are rendered
        // The Frame::set_cursor_position() is called there with the calculated position
    }
}
