use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, Paragraph, StatefulWidget, Widget},
};
use rustc_hash::FxHashMap;

use crate::agents::{
    ActionDisplay, ActionResult,
};
use crate::models::{ChatMessage, MessageRole};
use crate::tui::markdown::parse_markdown;
use crate::tui::theme::Theme;
use crate::utils::format_relative_timestamp;

/// State for the chat widget
#[derive(Debug, Clone)]
pub struct ChatState {
    /// Manual scroll offset (only used when is_user_scrolling = true)
    scroll_offset: u16,
    /// Whether user is manually scrolling (not following bottom)
    is_user_scrolling: bool,
}

impl ChatState {
    /// Create a new chat state (starts in auto-follow mode)
    pub fn new() -> Self {
        Self {
            scroll_offset: 0,
            is_user_scrolling: false,
        }
    }

    /// Get the scroll position for rendering
    /// scroll_offset represents distance from bottom, convert to ratatui scroll position
    pub fn get_scroll_position(&self, content_height: u16, viewport_height: u16) -> u16 {
        let max_scroll = content_height.saturating_sub(viewport_height);
        if self.is_user_scrolling {
            // Manual scroll: convert "distance from bottom" to scroll position
            // scroll_offset=0 → show bottom (max_scroll), scroll_offset=max → show top (0)
            let capped_offset = self.scroll_offset.min(max_scroll);
            max_scroll.saturating_sub(capped_offset)
        } else {
            // Auto-scroll: show bottom of content
            max_scroll
        }
    }

    /// Scroll viewport up (shows older messages further from bottom)
    pub fn scroll_up(&mut self, amount: u16) {
        self.is_user_scrolling = true;
        self.scroll_offset = self.scroll_offset.saturating_add(amount);
    }

    /// Scroll viewport down (shows newer messages closer to bottom)
    pub fn scroll_down(&mut self, amount: u16) {
        self.is_user_scrolling = true;
        self.scroll_offset = self.scroll_offset.saturating_sub(amount);
    }

    /// Force resume auto-scroll mode (jump to bottom)
    pub fn resume_auto_scroll(&mut self) {
        self.is_user_scrolling = false;
        self.scroll_offset = 0;
    }

    /// Check if user is manually scrolling (not following bottom)
    pub fn is_manually_scrolling(&self) -> bool {
        self.is_user_scrolling
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
    pub is_generating: bool,
    pub pending_file_read: bool,
    pub reading_file_status: Option<&'a str>,
    pub theme: &'a Theme,
    /// Shared markdown parse cache: (message_index, content_len) -> parsed lines
    pub markdown_cache: &'a mut FxHashMap<(usize, usize), Vec<Line<'static>>>,
}

impl<'a> StatefulWidget for ChatWidget<'a> {
    type State = ChatState;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        let mut lines = Vec::new();

        let message_count = self.messages.len();
        for (idx, msg) in self.messages.iter().enumerate() {
            // Skip Tool messages - they're internal to the agent loop and their
            // content is already displayed inline in the assistant's action blocks
            if matches!(msg.role, MessageRole::Tool) {
                continue;
            }

            let _is_last_message = idx == message_count - 1;
            let (role_prefix, role_color) = match msg.role {
                MessageRole::User => (">", ratatui::style::Color::White),
                MessageRole::Assistant => ("●", ratatui::style::Color::White),
                MessageRole::System => ("●", self.theme.colors.system_message.to_color()),
                MessageRole::Tool => continue, // Already handled above, but needed for exhaustive match
            };

            if matches!(msg.role, MessageRole::Assistant) {
                // Render thinking block if present
                if let Some(ref thinking) = msg.thinking {
                    // Add "Thinking..." header in italic and dimmed with grayed white dot
                    lines.push(Line::from(vec![
                        Span::styled(
                            "● ",
                            Style::new().fg(ratatui::style::Color::DarkGray),
                        ),
                        Span::styled(
                            "Thinking...",
                            Style::new()
                                .fg(self.theme.colors.text_secondary.to_color())
                                .italic()
                                .dim(),
                        ),
                    ]));

                    // Render thinking content with proper wrapping (2-space hanging indent)
                    let wrapped = wrap_text_with_indent(
                        thinking,
                        area.width as usize,
                        2, // first line indent (2 spaces)
                        2, // continuation indent (2 spaces)
                    );
                    for wrapped_line in wrapped {
                        lines.push(Line::from(Span::styled(
                            wrapped_line,
                            Style::new()
                                .fg(self.theme.colors.text_secondary.to_color())
                                .italic()
                                .dim(),
                        )));
                    }

                    // Add blank line after thinking block
                    lines.push(Line::from(""));
                }

                // With tool calling, message content is just text (no embedded action blocks)
                // Use cached parsed markdown when available (avoids re-parsing every frame)
                let cache_key = (idx, msg.content.len());
                let parsed_lines = if let Some(cached) = self.markdown_cache.get(&cache_key) {
                    cached.clone()
                } else {
                    let parsed = parse_markdown(&msg.content);
                    self.markdown_cache.insert(cache_key, parsed.clone());
                    parsed
                };

                for (line_idx, mut parsed_line) in parsed_lines.into_iter().enumerate() {
                    // Add role indicator to first line or 2-space margin to others
                    if line_idx == 0 {
                        // First line: prepend role indicator
                        let mut spans = vec![Span::styled(
                            format!("{} ", role_prefix),
                            Style::new().fg(role_color).bold(),
                        )];
                        spans.extend(parsed_line.spans);
                        parsed_line = Line::from(spans);
                    } else {
                        // Other lines: prepend 2-space margin
                        let mut spans = vec![Span::raw("  ")];
                        spans.extend(parsed_line.spans);
                        parsed_line = Line::from(spans);
                    }

                    // Wrap the styled line if needed (continuation indent = 2)
                    let wrapped = wrap_styled_line(parsed_line, area.width as usize, 2);
                    lines.extend(wrapped);
                }

                // Render all actions at the end of the message
                if !msg.actions.is_empty() {
                    render_actions(&msg.actions, &mut lines, self.theme);
                }
            } else {
                // For User messages: format timestamp and display on right edge
                let formatted_timestamp = format_relative_timestamp(msg.timestamp);
                let timestamp_width = formatted_timestamp.len() + 2; // +2 for spacing

                // Strip the [Sent at: ...] line from message content
                let cleaned_content = strip_timestamp_line(&msg.content);

                // Manually wrap the user message with hanging indent (2 spaces)
                let wrapped = wrap_text_with_indent(
                    &cleaned_content,
                    area.width as usize,
                    2, // first line indent (for role prefix "  ")
                    2, // continuation indent
                );

                for (line_idx, wrapped_line) in wrapped.iter().enumerate() {
                    if line_idx == 0 {
                        // First line: add role prefix and timestamp on right
                        let text_content = wrapped_line.trim_start(); // Remove the indent we added
                        let text_len = text_content.len();

                        let mut spans = vec![
                            Span::styled(
                                format!("{} ", role_prefix),
                                Style::new().fg(role_color).bold(),
                            ),
                            Span::raw(text_content.to_string()),
                        ];

                        // Add padding and timestamp on right
                        let content_width = role_prefix.len() + 1 + text_len;
                        let padding_needed = (area.width as usize).saturating_sub(content_width + timestamp_width);
                        if padding_needed > 0 {
                            spans.push(Span::raw(" ".repeat(padding_needed)));
                        }
                        spans.push(Span::styled(
                            formatted_timestamp.clone(),
                            Style::new().fg(ratatui::style::Color::Rgb(136, 136, 136)),
                        ));

                        lines.push(Line::from(spans));
                    } else {
                        // Continuation lines: already have 2-space margin from wrap_text_with_indent
                        lines.push(Line::from(wrapped_line.clone()));
                    }
                }
            }

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

        // NOTE: Wrapping is disabled because we handle it manually with hanging indents
        // Calculate content height and viewport for proper scroll clamping
        let content_height = lines.len() as u16;
        let viewport_height = area.height;

        let paragraph = Paragraph::new(lines)
            .block(Block::default())
            .scroll((state.get_scroll_position(content_height, viewport_height), 0));

        paragraph.render(area, buf);
    }
}

/// Render actions in Claude Code style
fn render_actions(actions: &[ActionDisplay], lines: &mut Vec<Line>, theme: &Theme) {
    for (action_idx, action) in actions.iter().enumerate() {
        // Add blank line between consecutive actions (not before first one)
        if action_idx > 0 {
            lines.push(Line::from(""));
        }
        let action_color = match action.action_type.as_str() {
            "Write" => theme.colors.success.to_color(),
            "Bash" | "Command" => theme.colors.info.to_color(),
            "Read" => theme.colors.info.to_color(),
            "Delete" => theme.colors.warning.to_color(),
            "GitDiff" | "GitStatus" | "GitCommit" => theme.colors.text_highlight.to_color(),
            "WebSearch" => theme.colors.info.to_color(),
            _ => theme.colors.text_secondary.to_color(),
        };

        lines.push(Line::from(vec![
            Span::styled("● ", Style::new().fg(action_color).bold()),
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
                    "WebSearch" => {
                        if let Some(ref preview) = action.preview {
                            preview.clone()
                        } else if let Some(count) = action.line_count {
                            format!("Web Search: {} results for '{}'", count, action.target)
                        } else {
                            format!("Web Search: {}", action.target)
                        }
                    },
                    _ => "Success".to_string(),
                };

                let result_lines: Vec<&str> = result_msg.lines().collect();
                for (idx, line) in result_lines.iter().enumerate() {
                    let prefix = if idx == 0 { "  ⎿ " } else { "    " };
                    lines.push(Line::from(vec![
                        Span::styled(prefix, Style::new().fg(action_color)),
                        Span::styled(
                            line.to_string(),
                            Style::new().fg(theme.colors.text_secondary.to_color()),
                        ),
                    ]));
                }

                // Show timing for long-running actions
                if let Some(duration) = action.duration_seconds {
                    // Only show timing for operations that typically take a few seconds
                    if matches!(
                        action.action_type.as_str(),
                        "WebSearch" | "Bash" | "Command" | "GitDiff" | "GitStatus" | "Read"
                    ) {
                        lines.push(Line::from(vec![
                            Span::styled("    ", Style::new().fg(action_color)),
                            Span::styled(
                                format!("Completed in {:.1} seconds", duration),
                                Style::new()
                                    .fg(theme.colors.text_disabled.to_color())
                                    .italic(),
                            ),
                        ]));
                    }
                }

                if action.action_type == "Write" {
                    if let Some(ref content) = action.file_content {
                        let preview_lines: Vec<&str> = content.lines().take(10).collect();
                        let total_lines = content.lines().count();

                        if !preview_lines.is_empty() {
                            lines.push(Line::from(vec![Span::styled(
                                "    ",
                                Style::new().fg(action_color),
                            )]));

                            let preview_content = preview_lines.join("\n");
                            let mut parsed =
                                parse_markdown(&format!("```\n{}\n```", preview_content));

                            for parsed_line in parsed.iter_mut() {
                                let mut new_spans =
                                    vec![Span::styled("    ", Style::new().fg(action_color))];
                                new_spans.extend(parsed_line.spans.drain(..));
                                parsed_line.spans = new_spans;
                            }

                            lines.extend(parsed);

                            if total_lines > 10 {
                                lines.push(Line::from(vec![
                                    Span::styled("    ", Style::new().fg(action_color)),
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
                    Span::styled("  ⎿ ", Style::new().fg(theme.colors.error.to_color())),
                    Span::styled(
                        format!("Error: {}", error),
                        Style::new().fg(theme.colors.error.to_color()),
                    ),
                ]));
            },
        }
    }
}

/// Strip the [Sent at: ...] timestamp line from message content
/// This line was added for the model but should be hidden from the user display
fn strip_timestamp_line(content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();

    if lines.is_empty() {
        return String::new();
    }

    // Check if first line is a timestamp marker
    if lines[0].starts_with("[Sent at:") && lines[0].ends_with("]") {
        // Skip the first line and rejoin remaining lines
        lines[1..].join("\n")
    } else {
        // No timestamp line to strip, return as-is
        content.to_string()
    }
}

/// Wrap text with hanging indent support
/// Returns a vector of strings, each representing a wrapped line
fn wrap_text_with_indent(text: &str, width: usize, first_line_indent: usize, continuation_indent: usize) -> Vec<String> {
    let mut wrapped_lines = Vec::new();

    for (line_idx, line) in text.lines().enumerate() {
        if line.is_empty() {
            wrapped_lines.push(String::new());
            continue;
        }

        let current_indent = if line_idx == 0 { first_line_indent } else { continuation_indent };
        let available_width = width.saturating_sub(current_indent);

        if available_width == 0 {
            wrapped_lines.push(" ".repeat(current_indent));
            continue;
        }

        let words: Vec<&str> = line.split_whitespace().collect();
        if words.is_empty() {
            wrapped_lines.push(" ".repeat(current_indent));
            continue;
        }

        let mut current_line = String::with_capacity(width);
        current_line.push_str(&" ".repeat(current_indent));
        let mut current_length = 0;

        for (word_idx, word) in words.iter().enumerate() {
            let word_len = word.len();

            if word_idx == 0 {
                // First word always fits on the line
                current_line.push_str(word);
                current_length = word_len;
            } else if current_length + 1 + word_len <= available_width {
                // Word fits on current line
                current_line.push(' ');
                current_line.push_str(word);
                current_length += 1 + word_len;
            } else {
                // Word doesn't fit, start a new line
                wrapped_lines.push(current_line);
                current_line = String::with_capacity(width);
                current_line.push_str(&" ".repeat(continuation_indent));
                current_line.push_str(word);
                current_length = word_len;
            }
        }

        // Add the last line
        if !current_line.trim().is_empty() {
            wrapped_lines.push(current_line);
        }
    }

    wrapped_lines
}

/// Wrap a styled Line with hanging indent, preserving all span styles
/// Returns multiple Line objects with proper indentation
fn wrap_styled_line(line: Line<'static>, width: usize, continuation_indent: usize) -> Vec<Line<'static>> {
    // Calculate the total length of the line by summing all span lengths
    let total_length: usize = line.spans.iter().map(|s| s.content.len()).sum();

    // If the line fits within width, return as-is
    if total_length <= width {
        return vec![line];
    }

    // Line needs wrapping - extract all text and styles
    let mut result_lines = Vec::new();
    let mut current_line_spans = Vec::new();
    let mut current_line_length = 0;
    let available_width = width.saturating_sub(continuation_indent);

    for span in line.spans.clone() {
        let span_text = span.content.to_string();
        let span_style = span.style;

        // Split span text by words
        let words: Vec<&str> = span_text.split_whitespace().collect();

        for (word_idx, word) in words.iter().enumerate() {
            let word_with_space = if word_idx > 0 || current_line_length > 0 {
                format!(" {}", word)
            } else {
                word.to_string()
            };

            let word_len = word_with_space.len();

            if current_line_length == 0 && result_lines.is_empty() {
                // First word of first line - no indent
                current_line_spans.push(Span::styled(word_with_space, span_style));
                current_line_length += word_len;
            } else if current_line_length + word_len <= available_width {
                // Word fits on current line
                current_line_spans.push(Span::styled(word_with_space, span_style));
                current_line_length += word_len;
            } else {
                // Word doesn't fit - finish current line and start new one
                result_lines.push(Line::from(current_line_spans));
                current_line_spans = vec![Span::raw(" ".repeat(continuation_indent))];
                current_line_spans.push(Span::styled(word.to_string(), span_style));
                current_line_length = word.len();
            }
        }
    }

    // Add the last line if it has content
    if !current_line_spans.is_empty() {
        result_lines.push(Line::from(current_line_spans));
    }

    if result_lines.is_empty() {
        vec![line]
    } else {
        result_lines
    }
}
