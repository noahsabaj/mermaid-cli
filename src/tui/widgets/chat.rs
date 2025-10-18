use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, StatefulWidget, Widget, Wrap},
};

use crate::agents::{
    segment_message, strip_action_blocks, ActionDisplay, ActionResult, MessageSegment,
};
use crate::models::{ChatMessage, MessageRole};
use crate::tui::app::ConfirmationState;
use crate::tui::markdown::parse_markdown;
use crate::tui::mode::OperationMode;
use crate::tui::theme::Theme;

/// State for the chat widget
#[derive(Debug, Clone)]
pub struct ChatState {
    /// Current scroll offset in the chat view
    pub scroll_offset: u16,
    /// Whether user is manually scrolling (not at bottom)
    pub is_user_scrolling: bool,
}

impl ChatState {
    /// Create a new chat state
    pub fn new() -> Self {
        Self {
            scroll_offset: 0,
            is_user_scrolling: false,
        }
    }

    /// Calculate the maximum scroll offset (bottom of content)
    pub fn calculate_max_scroll(
        &self,
        messages: &[ChatMessage],
        current_response: &str,
        is_generating: bool,
        viewport_height: u16,
    ) -> u16 {
        let mut total_lines = 0u16;

        for msg in messages {
            // Role line
            total_lines += 1;
            // Content lines
            total_lines += msg.content.lines().count() as u16;
            // Spacing
            if matches!(msg.role, MessageRole::Assistant) {
                total_lines += 1;
            }
            // Empty line between messages
            total_lines += 1;
        }

        // Add lines for current response if generating
        if is_generating && !current_response.is_empty() {
            total_lines += 1; // Role line
            total_lines += current_response.lines().count() as u16;
            total_lines += 1; // Typing indicator
        }

        // Max scroll is total lines minus viewport height
        total_lines.saturating_sub(viewport_height)
    }

    /// Auto-scroll to bottom of chat
    pub fn auto_scroll_to_bottom(
        &mut self,
        messages: &[ChatMessage],
        current_response: &str,
        is_generating: bool,
        viewport_height: u16,
    ) {
        if !self.is_user_scrolling {
            self.scroll_offset = self.calculate_max_scroll(
                messages,
                current_response,
                is_generating,
                viewport_height,
            );
        }
    }

    /// Scroll chat view up
    pub fn scroll_up(
        &mut self,
        amount: u16,
        messages: &[ChatMessage],
        current_response: &str,
        is_generating: bool,
        viewport_height: u16,
    ) {
        let max_scroll =
            self.calculate_max_scroll(messages, current_response, is_generating, viewport_height);
        self.scroll_offset = self.scroll_offset.saturating_add(amount).min(max_scroll);

        // User is manually scrolling if they're not at the bottom
        let threshold = 3;
        if self.scroll_offset < max_scroll.saturating_sub(threshold) {
            self.is_user_scrolling = true;
        }
    }

    /// Scroll chat view down
    pub fn scroll_down(
        &mut self,
        amount: u16,
        messages: &[ChatMessage],
        current_response: &str,
        is_generating: bool,
        viewport_height: u16,
    ) {
        self.scroll_offset = self.scroll_offset.saturating_sub(amount);

        // If user scrolls close to bottom, resume auto-scrolling
        let max_scroll =
            self.calculate_max_scroll(messages, current_response, is_generating, viewport_height);
        let threshold = 3;
        if self.scroll_offset >= max_scroll.saturating_sub(threshold) {
            self.is_user_scrolling = false;
            self.scroll_offset = max_scroll;
        }
    }
}

impl Default for ChatState {
    fn default() -> Self {
        Self::new()
    }
}

/// Props for ChatWidget
pub struct ChatWidget<'a> {
    pub messages: &'a [ChatMessage],
    pub current_response: &'a str,
    pub is_generating: bool,
    pub confirmation_state: Option<&'a ConfirmationState>,
    pub pending_file_read: bool,
    pub reading_file_status: Option<&'a str>,
    pub operation_mode: OperationMode,
    pub theme: &'a Theme,
}

impl<'a> StatefulWidget for ChatWidget<'a> {
    type State = ChatState;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        let mut lines = Vec::new();

        let message_count = self.messages.len();
        for (idx, msg) in self.messages.iter().enumerate() {
            let _is_last_message = idx == message_count - 1;
            let (role_prefix, role_color) = match msg.role {
                MessageRole::User => (">", self.theme.colors.user_message.to_color()),
                MessageRole::Assistant => ("●", self.theme.colors.assistant_message.to_color()),
                MessageRole::System => ("●", self.theme.colors.system_message.to_color()),
            };

            if matches!(msg.role, MessageRole::Assistant) {
                let segments = segment_message(&msg.content);
                let mut is_first_segment = true;

                for segment in segments {
                    match segment {
                        MessageSegment::Text(text) => {
                            let mut parsed_lines = parse_markdown(&text);

                            if is_first_segment {
                                if let Some(first_line) = parsed_lines.first_mut() {
                                    let mut spans = vec![Span::styled(
                                        format!("{} ", role_prefix),
                                        Style::new().fg(role_color).bold(),
                                    )];
                                    spans.extend(first_line.spans.drain(..));
                                    first_line.spans = spans;
                                }
                                is_first_segment = false;
                            }

                            lines.extend(parsed_lines);
                        },
                        MessageSegment::ActionMarker {
                            action_type,
                            target,
                        } => {
                            if let Some(action_display) = msg
                                .actions
                                .iter()
                                .find(|a| a.action_type == action_type && a.target == target)
                            {
                                let actions_to_render = vec![action_display.clone()];
                                render_actions(&actions_to_render, &mut lines, self.theme);
                            }
                            is_first_segment = false;
                        },
                    }
                }
            } else {
                let content_lines: Vec<_> = msg.content.lines().collect();
                if let Some(first) = content_lines.first() {
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!("{} ", role_prefix),
                            Style::new().fg(role_color).bold(),
                        ),
                        Span::raw(first.to_string()),
                    ]));

                    for line in content_lines.iter().skip(1) {
                        lines.push(Line::from(line.to_string()));
                    }
                }
            }

            lines.push(Line::from(""));
        }

        // Show inline confirmation box if action pending
        if let Some(confirmation) = self.confirmation_state {
            let width = area.width.saturating_sub(4) as usize;

            lines.push(Line::from("╔".to_string() + &"═".repeat(width - 2) + "╗"));

            let title = format!("Action: {}", confirmation.action_description);
            let title_len = title.len();
            lines.push(Line::from(vec![
                Span::raw("║ "),
                Span::styled(
                    title,
                    Style::new().fg(self.theme.colors.warning.to_color()).bold(),
                ),
                Span::raw(format!(
                    "{}║",
                    " ".repeat(width.saturating_sub(title_len + 3))
                )),
            ]));

            if let Some(ref info) = confirmation.file_info {
                lines.push(Line::from("╠".to_string() + &"─".repeat(width - 2) + "╣"));

                let path_line = format!("   File: {}", info.path);
                let path_line_len = path_line.len();
                lines.push(Line::from(vec![
                    Span::raw("║"),
                    Span::raw(path_line),
                    Span::raw(format!(
                        "{}║",
                        " ".repeat(width.saturating_sub(path_line_len + 1))
                    )),
                ]));

                let status = if info.exists {
                    "Will overwrite"
                } else {
                    "New file"
                };
                let size_line = format!("   Size: {} bytes | {}", info.size, status);
                let size_line_len = size_line.len();
                lines.push(Line::from(vec![
                    Span::raw("║"),
                    Span::raw(size_line),
                    Span::raw(format!(
                        "{}║",
                        " ".repeat(width.saturating_sub(size_line_len + 1))
                    )),
                ]));

                if let Some(ref lang) = info.language {
                    let lang_line = format!("   Type: {}", lang);
                    let lang_line_len = lang_line.len();
                    lines.push(Line::from(vec![
                        Span::raw("║"),
                        Span::raw(lang_line),
                        Span::raw(format!(
                            "{}║",
                            " ".repeat(width.saturating_sub(lang_line_len + 1))
                        )),
                    ]));
                }

                if !confirmation.preview_lines.is_empty() {
                    lines.push(Line::from("╠".to_string() + &"─".repeat(width - 2) + "╣"));
                    lines.push(Line::from(vec![
                        Span::raw("║ Preview:"),
                        Span::raw(format!("{}║", " ".repeat(width.saturating_sub(10)))),
                    ]));
                    for line in confirmation.preview_lines.iter().take(3) {
                        let preview_line = format!("   {}", line);
                        let truncated = if preview_line.len() > width - 2 {
                            format!("{}...", &preview_line[..width - 5])
                        } else {
                            preview_line
                        };
                        let truncated_len = truncated.len();
                        lines.push(Line::from(vec![
                            Span::raw("║"),
                            Span::styled(
                                truncated,
                                Style::new().fg(self.theme.colors.text_secondary.to_color()),
                            ),
                            Span::raw(format!(
                                "{}║",
                                " ".repeat(width.saturating_sub(truncated_len + 1))
                            )),
                        ]));
                    }
                    if confirmation.preview_lines.len() > 3 {
                        lines.push(Line::from(vec![
                            Span::raw("║   ..."),
                            Span::raw(format!("{}║", " ".repeat(width.saturating_sub(7)))),
                        ]));
                    }
                }
            }

            lines.push(Line::from("╠".to_string() + &"═".repeat(width - 2) + "╣"));

            let shortcuts = if confirmation.allow_always {
                " [Alt+Y] Approve   [Alt+N] Skip   [Alt+A] Always   [Alt+P] Preview "
            } else {
                " [Alt+Y] Approve   [Alt+N] Skip   [Alt+P] Preview "
            };

            let padding = (width.saturating_sub(shortcuts.len())) / 2;
            lines.push(Line::from(vec![
                Span::raw("║"),
                Span::raw(" ".repeat(padding)),
                Span::styled(
                    shortcuts,
                    Style::new().fg(self.theme.colors.info.to_color()).bold(),
                ),
                Span::raw(format!(
                    "{}║",
                    " ".repeat(width.saturating_sub(shortcuts.len() + padding + 1))
                )),
            ]));

            lines.push(Line::from("╚".to_string() + &"═".repeat(width - 2) + "╝"));
            lines.push(Line::from(""));
        }

        // Show file reading status if active
        if self.pending_file_read && self.reading_file_status.is_some() && !self.is_generating {
            if let Some(status) = self.reading_file_status {
                lines.push(Line::from(vec![
                    Span::styled(
                        "  [READ] ",
                        Style::new().fg(self.theme.colors.info.to_color()).bold(),
                    ),
                    Span::styled(
                        status,
                        Style::new()
                            .fg(self.theme.colors.info.to_color())
                            .italic()
                            .slow_blink(),
                    ),
                ]));
                lines.push(Line::from(""));
            }
        }

        // NOTE: current_response is NOT rendered during streaming (buffering mode).
        // The response is buffered invisibly and only shown when generation is complete.
        // This provides a Claude Code-like experience where the complete response
        // appears instantly instead of streaming character-by-character.
        //
        // The status line shows progress: "↑ Sending..." → "↓ Streaming..." with timer

        let paragraph = Paragraph::new(lines)
            .block(Block::default())
            .wrap(Wrap { trim: false })
            .scroll((state.scroll_offset, 0));

        paragraph.render(area, buf);
    }
}

/// Render actions in Claude Code style
fn render_actions(actions: &[ActionDisplay], lines: &mut Vec<Line>, theme: &Theme) {
    for action in actions {
        let action_color = match action.action_type.as_str() {
            "Write" => theme.colors.success.to_color(),
            "Bash" | "Command" => theme.colors.info.to_color(),
            "Read" => theme.colors.info.to_color(),
            "Delete" => theme.colors.warning.to_color(),
            "GitDiff" | "GitStatus" | "GitCommit" => theme.colors.text_highlight.to_color(),
            _ => theme.colors.text_secondary.to_color(),
        };

        lines.push(Line::from(vec![
            Span::styled("  ● ", Style::new().fg(action_color).bold()),
            Span::styled(
                format!("{}(", action.action_type),
                Style::new().fg(action_color).bold(),
            ),
            Span::styled(
                action.target.clone(),
                Style::new().fg(theme.colors.text_secondary.to_color()),
            ),
            Span::styled(")", Style::new().fg(action_color).bold()),
        ]));

        match &action.result {
            ActionResult::Success { .. } => {
                let result_msg = match action.action_type.as_str() {
                    "Write" => {
                        if let Some(count) = action.line_count {
                            format!("Wrote {} lines to {}", count, action.target)
                        } else {
                            format!("Wrote {}", action.target)
                        }
                    },
                    "Read" => {
                        if let Some(count) = action.line_count {
                            format!("Read {} lines from {}", count, action.target)
                        } else {
                            format!("Read {}", action.target)
                        }
                    },
                    "Bash" | "Command" => {
                        if let Some(ref preview) = action.preview {
                            preview.clone()
                        } else if let Some(count) = action.line_count {
                            format!("Command output: {} lines", count)
                        } else {
                            "Command executed successfully".to_string()
                        }
                    },
                    "Delete" => format!("Deleted {}", action.target),
                    "CreateDir" => format!("Created directory {}", action.target),
                    "GitDiff" | "GitStatus" | "GitCommit" => {
                        if let Some(ref preview) = action.preview {
                            preview.clone()
                        } else {
                            "Operation completed".to_string()
                        }
                    },
                    _ => "Success".to_string(),
                };

                let result_lines: Vec<&str> = result_msg.lines().collect();
                for (idx, line) in result_lines.iter().enumerate() {
                    let prefix = if idx == 0 { "    ⎿ " } else { "      " };
                    lines.push(Line::from(vec![
                        Span::styled(prefix, Style::new().fg(action_color)),
                        Span::styled(
                            line.to_string(),
                            Style::new().fg(theme.colors.text_secondary.to_color()),
                        ),
                    ]));
                }

                if action.action_type == "Write" {
                    if let Some(ref content) = action.file_content {
                        let preview_lines: Vec<&str> = content.lines().take(10).collect();
                        let total_lines = content.lines().count();

                        if !preview_lines.is_empty() {
                            lines.push(Line::from(vec![Span::styled(
                                "      ",
                                Style::new().fg(action_color),
                            )]));

                            let preview_content = preview_lines.join("\n");
                            let mut parsed =
                                parse_markdown(&format!("```\n{}\n```", preview_content));

                            for parsed_line in parsed.iter_mut() {
                                let mut new_spans =
                                    vec![Span::styled("      ", Style::new().fg(action_color))];
                                new_spans.extend(parsed_line.spans.drain(..));
                                parsed_line.spans = new_spans;
                            }

                            lines.extend(parsed);

                            if total_lines > 10 {
                                lines.push(Line::from(vec![
                                    Span::styled("      ", Style::new().fg(action_color)),
                                    Span::styled(
                                        format!("... ({} more lines)", total_lines - 10),
                                        Style::new()
                                            .fg(theme.colors.text_disabled.to_color())
                                            .italic(),
                                    ),
                                ]));
                            }
                        }
                    }
                }
            },
            ActionResult::Error { error } => {
                lines.push(Line::from(vec![
                    Span::styled("    ⎿ ", Style::new().fg(theme.colors.error.to_color())),
                    Span::styled(
                        format!("Error: {}", error),
                        Style::new().fg(theme.colors.error.to_color()),
                    ),
                ]));
            },
        }
    }
}
