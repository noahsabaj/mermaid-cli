use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, StatefulWidget, Widget},
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
        Self { cursor_position: 0 }
    }

    /// Calculate cursor position for wrapped text
    pub fn calculate_cursor_position(
        input: &str,
        cursor_pos: usize,
        inner_width: usize,
    ) -> (u16, u16) {
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

    fn render(self, area: Rect, buf: &mut Buffer, _state: &mut Self::State) {
        let (_hints_area, input_area) = if self.showing_command_hints {
            let commands = vec![
                (":quit", "Quit the application"),
                (":q", "Quit (shortcut)"),
                (":clear", "Clear chat history"),
                (":model [name]", "Switch model or show current"),
                (":refresh", "Refresh file context from disk"),
                (":r", "Refresh (shortcut)"),
                (":save [name]", "Save current conversation"),
                (":load [name]", "Load a conversation"),
                (":list", "List saved conversations"),
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
                        Style::new().fg(self.theme.colors.info.to_color()).bold(),
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

        // Manually wrap input text with proper indentation (Claude Code style)
        // First line: "> text"
        // Continuation lines: "  text" (2 spaces to align with first line content)
        // Always show "> " prompt, even when input is empty
        let input_text = {
            let width = input_area.width.saturating_sub(2) as usize; // Account for top/bottom borders
            wrap_input_with_prompt(self.input, width)
        };

        // Build block - only add title when showing command hints
        // Use top and bottom borders to span full width like Claude Code
        let block = if self.showing_command_hints {
            Block::default()
                .borders(Borders::TOP | Borders::BOTTOM)
                .border_style(Style::new().fg(self.theme.colors.warning.to_color()))
                .title(" Enter Command ")
        } else {
            Block::default()
                .borders(Borders::TOP | Borders::BOTTOM)
                .border_style(Style::new().fg(self.theme.colors.border.to_color()))
        };

        let input = Paragraph::new(input_text)
            .style(input_style)
            .block(block);

        input.render(input_area, buf);

        // Note: Cursor positioning is handled in the main render loop after all widgets are rendered
        // The Frame::set_cursor_position() is called there with the calculated position
    }
}

/// Wrap input text with "> " prefix on first line and "  " on continuation lines (Claude Code style)
/// Always returns at least "> " even when input is empty
fn wrap_input_with_prompt(input: &str, width: usize) -> String {
    if width < 3 {
        // Not enough space for "> " prefix
        return input.to_string();
    }

    // Always start with "> " prompt
    let mut result = String::from("> ");

    // If input is empty, just return the prompt
    if input.is_empty() {
        return result;
    }

    let first_line_width = width.saturating_sub(2); // Reserve 2 chars for "> "
    let continuation_width = width.saturating_sub(2); // Reserve 2 chars for "  "

    let mut chars_remaining = input;
    let mut is_first_line = true;

    while !chars_remaining.is_empty() {
        let line_width = if is_first_line {
            first_line_width
        } else {
            continuation_width
        };

        // Find where to break the line
        let break_point = if chars_remaining.len() <= line_width {
            // Entire remaining text fits on this line
            chars_remaining.len()
        } else {
            // Find last space before width limit for nice wrapping
            chars_remaining[..line_width]
                .rfind(char::is_whitespace)
                .map(|pos| pos + 1) // Include the space
                .unwrap_or(line_width) // No space found, hard break
        };

        // Extract this line's text
        let line_text = &chars_remaining[..break_point];

        // Add line text (prefix already added for first line, or add it for continuation)
        if is_first_line {
            // First line: "> " already in result, just add the text
            result.push_str(line_text.trim_end());
        } else {
            // Continuation line: add newline + "  " prefix + text
            result.push('\n');
            result.push_str("  ");
            result.push_str(line_text.trim_end());
        }

        // Move to next line
        chars_remaining = &chars_remaining[break_point..].trim_start();
        is_first_line = false;
    }

    result
}
