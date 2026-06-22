use std::hash::{Hash, Hasher};

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph, StatefulWidget, Widget},
};
use rustc_hash::FxHashMap;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::domain::{ActionDetails, ActionDisplay, ActionResult, format_compact_count};
use crate::models::ChatMessageKind;
use crate::models::{ChatMessage, MessageRole};
use crate::render::diff::{DiffLineKind, parse_diff_line};
use crate::render::markdown::parse_markdown;
use crate::render::theme::Theme;
use crate::utils::format_relative_timestamp;

/// Entry in the click map: maps a content line to an image in chat history
#[derive(Debug, Clone)]
pub struct ImageClickTarget {
    /// Index into session_state.messages
    pub message_index: usize,
    /// Index into that message's images vec
    pub image_index: usize,
}

/// State for the chat widget
#[derive(Debug, Clone)]
pub struct ChatState {
    /// Manual scroll offset (only used when is_user_scrolling = true)
    scroll_offset: u16,
    /// Whether user is manually scrolling (not following bottom)
    is_user_scrolling: bool,
    /// Click map: content line number → image target (rebuilt every render)
    pub image_click_map: Vec<(u16, ImageClickTarget)>,
    /// Scroll position used in last render (for coordinate mapping)
    pub last_scroll_position: u16,
    /// Chat area rect from last render
    pub last_chat_area: Option<(u16, u16, u16, u16)>, // (x, y, width, height)
    /// Active drag-selection in CONTENT coordinates: `(anchor, cursor)` where
    /// each is `(content_line, col_cells)`. Highlight + copy derive from it.
    selection: Option<((usize, usize), (usize, usize))>,
    /// Plain text of each rendered content row, captured every frame so the
    /// selection can be extracted by display-cell range. Indexed by content
    /// line (the same index the selection uses).
    last_rendered_rows: Vec<String>,
}

impl ChatState {
    /// Create a new chat state (starts in auto-follow mode)
    pub fn new() -> Self {
        Self {
            scroll_offset: 0,
            is_user_scrolling: false,
            image_click_map: Vec::new(),
            last_scroll_position: 0,
            last_chat_area: None,
            selection: None,
            last_rendered_rows: Vec::new(),
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
        // A selection's content-line anchors don't track scrolling; drop it
        // rather than leave a highlight stranded on the wrong rows.
        self.selection = None;
    }

    /// Scroll viewport down (shows newer messages closer to bottom)
    /// Automatically resumes auto-scroll when reaching the bottom
    pub fn scroll_down(&mut self, amount: u16) {
        self.scroll_offset = self.scroll_offset.saturating_sub(amount);
        if self.scroll_offset == 0 {
            // Reached bottom — resume auto-follow mode
            self.is_user_scrolling = false;
        }
        self.selection = None;
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

    /// Find an image click target at the given screen coordinates.
    /// Returns Some((message_index, image_index)) if an image indicator was clicked.
    pub fn find_image_at_screen_pos(&self, screen_row: u16) -> Option<&ImageClickTarget> {
        let (_, area_y, _, area_height) = self.last_chat_area?;

        // Check if click is within chat area
        if screen_row < area_y || screen_row >= area_y + area_height {
            return None;
        }

        // Convert screen row to content line
        let viewport_row = screen_row - area_y;
        let content_line = viewport_row + self.last_scroll_position;

        // Look up in click map
        self.image_click_map
            .iter()
            .find(|(line, _)| *line == content_line)
            .map(|(_, target)| target)
    }

    /// Map a screen `(row, col)` to content `(line, col_cells)`, or `None`
    /// when the point is outside the chat area. `col` is clamped to the chat
    /// area's left edge so a drag past the gutter still maps to column 0.
    fn screen_to_content(&self, screen_row: u16, screen_col: u16) -> Option<(usize, usize)> {
        let (area_x, area_y, _, area_height) = self.last_chat_area?;
        if screen_row < area_y || screen_row >= area_y + area_height {
            return None;
        }
        let content_line = (screen_row - area_y) as usize + self.last_scroll_position as usize;
        let col = screen_col.saturating_sub(area_x) as usize;
        Some((content_line, col))
    }

    /// Begin a drag selection at the given screen position (mouse-down).
    /// Anchors and cursor both start here; a plain click with no drag selects
    /// nothing.
    pub fn begin_selection(&mut self, screen_row: u16, screen_col: u16) {
        self.selection = self
            .screen_to_content(screen_row, screen_col)
            .map(|p| (p, p));
    }

    /// Extend the in-progress selection to the given screen position (drag).
    pub fn update_selection(&mut self, screen_row: u16, screen_col: u16) {
        if let Some((anchor, _)) = self.selection
            && let Some(cursor) = self.screen_to_content(screen_row, screen_col)
        {
            self.selection = Some((anchor, cursor));
        }
    }

    /// Drop any active selection (and its highlight).
    pub fn clear_selection(&mut self) {
        self.selection = None;
    }

    /// Extract the currently-selected text from the last rendered frame, or
    /// `None` if there's no selection or it's empty (e.g. a plain click).
    /// Walks the retained per-row text and slices each row by display cells so
    /// CJK / wide glyphs are never split mid-cell.
    pub fn selected_text(&self) -> Option<String> {
        let (a, b) = self.selection?;
        let (start, end) = if a <= b { (a, b) } else { (b, a) };
        if self.last_rendered_rows.is_empty() {
            return None;
        }
        let last = self.last_rendered_rows.len() - 1;
        let (start_line, start_col) = (start.0.min(last), start.1);
        let (end_line, end_col) = (end.0.min(last), end.1);

        let mut out = String::new();
        for line in start_line..=end_line {
            let row = &self.last_rendered_rows[line];
            let c0 = if line == start_line { start_col } else { 0 };
            let c1 = if line == end_line {
                end_col
            } else {
                usize::MAX
            };
            out.push_str(slice_by_cells(row, c0, c1).trim_end());
            if line != end_line {
                out.push('\n');
            }
        }
        if out.is_empty() { None } else { Some(out) }
    }
}

/// Byte offset in `s` at the start of display-cell `target` (clamped to
/// `s.len()`). A wide glyph straddling `target` is kept whole on the right
/// side, so slicing never lands mid-character.
fn byte_at_cell(s: &str, target: usize) -> usize {
    if target == 0 {
        return 0;
    }
    let mut width = 0usize;
    for (idx, ch) in s.char_indices() {
        if width >= target {
            return idx;
        }
        width += ch.width().unwrap_or(0);
    }
    s.len()
}

/// Slice `s` to the display-cell range `[c0, c1)`.
fn slice_by_cells(s: &str, c0: usize, c1: usize) -> &str {
    let start = byte_at_cell(s, c0);
    let end = byte_at_cell(s, c1).max(start);
    &s[start..end]
}

/// The plain text of a rendered line (spans concatenated, styles dropped).
fn line_plain_text(line: &Line) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// Apply `hl` (merged onto each span's existing style) to display cells
/// `[c0, c1)` of `line`, splitting spans at the selection boundaries so the
/// highlight lands on exactly the selected glyphs.
fn highlight_line_cells(line: &mut Line<'static>, c0: usize, c1: usize, hl: Style) {
    let mut new_spans: Vec<Span<'static>> = Vec::with_capacity(line.spans.len() + 2);
    let mut width = 0usize;
    for span in line.spans.drain(..) {
        let span_w = span.content.width();
        let (span_start, span_end) = (width, width + span_w);
        width = span_end;

        let ov0 = c0.max(span_start);
        let ov1 = c1.min(span_end);
        if ov1 <= ov0 {
            new_spans.push(span); // no overlap with the selection
            continue;
        }

        let s = span.content.as_ref();
        let b0 = byte_at_cell(s, ov0 - span_start);
        let b1 = byte_at_cell(s, ov1 - span_start);
        if b0 > 0 {
            new_spans.push(Span::styled(s[..b0].to_string(), span.style));
        }
        new_spans.push(Span::styled(s[b0..b1].to_string(), span.style.patch(hl)));
        if b1 < s.len() {
            new_spans.push(Span::styled(s[b1..].to_string(), span.style));
        }
    }
    line.spans = new_spans;
}

impl Default for ChatState {
    fn default() -> Self {
        Self::new()
    }
}

/// Props for ChatWidget
pub struct ChatWidget<'a> {
    pub messages: &'a [ChatMessage],
    pub theme: &'a Theme,
    /// Shared markdown parse cache: content hash → parsed lines.
    pub markdown_cache: &'a mut FxHashMap<u64, Vec<Line<'static>>>,
    pub show_reasoning: bool,
}

impl<'a> StatefulWidget for ChatWidget<'a> {
    type State = ChatState;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        let mut lines: Vec<Line<'static>> = Vec::new();

        // Clear click map for this render pass
        state.image_click_map.clear();
        state.last_chat_area = Some((area.x, area.y, area.width, area.height));

        let mut hidden_reasoning_notice_shown = false;

        for (idx, msg) in self.messages.iter().enumerate() {
            // Skip Tool messages - they're internal to the agent loop and their
            // content is already displayed inline in the assistant's action blocks
            if matches!(msg.role, MessageRole::Tool) {
                continue;
            }

            if matches!(msg.kind, ChatMessageKind::ContextCheckpoint) {
                if let Some(event_lines) = render_context_checkpoint_event(msg, self.theme) {
                    lines.extend(event_lines);
                    lines.push(Line::from(""));
                }
                continue;
            }

            let (role_prefix, role_color) = match msg.role {
                MessageRole::User => (">", ratatui::style::Color::White),
                MessageRole::Assistant => ("●", ratatui::style::Color::White),
                MessageRole::System => ("●", self.theme.colors.system_message.to_color()),
                MessageRole::Tool => unreachable!("Tool messages filtered above"),
            };

            if matches!(msg.role, MessageRole::Assistant) {
                // Render thinking block if present
                if let Some(ref thinking) = msg.thinking {
                    // Skip rendering if thinking content is empty or literal "None"
                    let thinking_trimmed = thinking.trim();
                    if thinking_trimmed.is_empty()
                        || thinking_trimmed == "None"
                        || thinking_trimmed == "none"
                    {
                        // Don't render empty/null thinking blocks
                    } else if self.show_reasoning {
                        // Add "Thinking..." header in italic and dimmed with grayed white dot
                        lines.push(Line::from(vec![
                            Span::styled("● ", Style::new().fg(ratatui::style::Color::DarkGray)),
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
                    } else if !hidden_reasoning_notice_shown {
                        hidden_reasoning_notice_shown = true;
                        let marker = if msg.content.trim().is_empty() && msg.actions.is_empty() {
                            "Reasoning-only response hidden (/visible-reasoning on to show)"
                        } else {
                            "Reasoning hidden (/visible-reasoning on to show)"
                        };
                        lines.push(Line::from(vec![
                            Span::styled("● ", Style::new().fg(ratatui::style::Color::DarkGray)),
                            Span::styled(
                                marker,
                                Style::new()
                                    .fg(self.theme.colors.text_secondary.to_color())
                                    .italic()
                                    .dim(),
                            ),
                        ]));
                        // Always separate the placeholder from whatever follows
                        // (tool actions or the visible answer) with one blank
                        // line — the same gap every other block gets. Without
                        // this, a "thought, then called a tool" turn (empty
                        // content + actions) rendered the placeholder flush
                        // against the first "● Bash(…)" line.
                        lines.push(Line::from(""));
                        if msg.content.trim().is_empty() && msg.actions.is_empty() {
                            continue;
                        }
                    } else if msg.content.trim().is_empty() && msg.actions.is_empty() {
                        continue;
                    }
                }

                // With tool calling, message content is just text (no embedded action blocks)
                // Use cached parsed markdown when available (avoids re-parsing every frame)
                let mut hasher = rustc_hash::FxHasher::default();
                msg.content.hash(&mut hasher);
                let cache_key = hasher.finish();
                let parsed_lines = if let Some(cached) = self.markdown_cache.get(&cache_key) {
                    cached.clone()
                } else {
                    let parsed = parse_markdown(&msg.content);
                    self.markdown_cache.insert(cache_key, parsed.clone());
                    if self.markdown_cache.len() > crate::constants::MARKDOWN_CACHE_MAX_ENTRIES {
                        self.markdown_cache.clear();
                        self.markdown_cache.insert(cache_key, parsed.clone());
                    }
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
                    // Add blank line between text content and actions
                    if !msg.content.trim().is_empty() {
                        lines.push(Line::from(""));
                    }
                    render_actions(&msg.actions, &mut lines, self.theme, area.width as usize);
                }
            } else {
                // For User messages: format timestamp and display on right edge
                let formatted_timestamp = format_relative_timestamp(msg.timestamp);
                let timestamp_len = formatted_timestamp.len();
                let min_gap = 3; // minimum spaces between text and timestamp

                // Content is clean — timestamps are injected at API call time only
                let cleaned_content = &msg.content;

                // Reserve space on the first line for role prefix + gap + timestamp
                // so text wraps early enough to not overlap the timestamp
                let role_prefix_width = role_prefix.len() + 1; // "You " = prefix + space
                let first_line_reserved = role_prefix_width + min_gap + timestamp_len;

                // Manually wrap the user message with hanging indent (2 spaces)
                let wrapped = wrap_text_with_indent(
                    cleaned_content,
                    area.width as usize,
                    first_line_reserved, // reserve space for prefix + gap + timestamp on first line
                    2,                   // continuation indent
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

                        // Always add at least min_gap spaces, plus any extra from word-boundary slack
                        let content_width = role_prefix_width + text_len;
                        let total_used = content_width + min_gap + timestamp_len;
                        let extra_padding = (area.width as usize).saturating_sub(total_used);
                        spans.push(Span::raw(" ".repeat(min_gap + extra_padding)));
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

            // Show image indicators under user and assistant messages.
            // User images come from clipboard paste (`Attachment`); assistant
            // images come from tool executions that emitted `ProgressEvent::
            // Artifact` during their run — screenshot captures, inline
            // previews from computer-use, etc. Both land in `msg.images` as
            // base64 strings and render the same way.
            if matches!(msg.role, MessageRole::User | MessageRole::Assistant)
                && let Some(ref images) = msg.images
                && !images.is_empty()
            {
                for (i, _) in images.iter().enumerate() {
                    // Record this line in the click map before pushing
                    let content_line = lines.len() as u16;
                    state.image_click_map.push((
                        content_line,
                        ImageClickTarget {
                            message_index: idx,
                            image_index: i,
                        },
                    ));
                    lines.push(Line::from(vec![
                        Span::styled("  ⎿ ", Style::new().fg(self.theme.colors.info.to_color())),
                        Span::styled(
                            format!("[Image #{}]", i + 1),
                            Style::new().fg(self.theme.colors.info.to_color()).italic(),
                        ),
                    ]));
                }
            }

            lines.push(Line::from(""));
        }

        // NOTE: The response buffer is NOT rendered during streaming (buffering mode).
        // The response is buffered invisibly and only shown when generation is complete.
        // This provides a Claude Code-like experience where the complete response
        // appears instantly instead of streaming character-by-character.
        //
        // The status line shows progress: "↑ Sending..." → "↓ Streaming..." with timer

        // Capture the plain text of each rendered row for selection
        // extraction. Done before the highlight pass, which only changes
        // styling, not text.
        state.last_rendered_rows = lines.iter().map(line_plain_text).collect();

        // Paint the active drag selection (reverse video over the selected
        // cells). Selection lines are content indices, matching `lines`.
        if let Some((a, b)) = state.selection
            && !lines.is_empty()
        {
            let (start, end) = if a <= b { (a, b) } else { (b, a) };
            let sel_style = Style::new().add_modifier(Modifier::REVERSED);
            let last_line = lines.len() - 1;
            for (line_idx, line) in lines
                .iter_mut()
                .enumerate()
                .take(end.0.min(last_line) + 1)
                .skip(start.0)
            {
                let c0 = if line_idx == start.0 { start.1 } else { 0 };
                let c1 = if line_idx == end.0 { end.1 } else { usize::MAX };
                if c1 > c0 {
                    highlight_line_cells(line, c0, c1, sel_style);
                }
            }
        }

        // NOTE: Wrapping is disabled because we handle it manually with hanging indents
        // Calculate content height and viewport for proper scroll clamping
        let content_height = lines.len() as u16;
        let viewport_height = area.height;

        let scroll_pos = state.get_scroll_position(content_height, viewport_height);
        state.last_scroll_position = scroll_pos;

        let paragraph = Paragraph::new(lines)
            .block(Block::default())
            .scroll((scroll_pos, 0));

        paragraph.render(area, buf);
    }
}

fn render_context_checkpoint_event(msg: &ChatMessage, theme: &Theme) -> Option<Vec<Line<'static>>> {
    if !matches!(msg.role, MessageRole::User) {
        return None;
    }

    let metadata = msg.metadata.as_ref();
    let trigger = metadata
        .and_then(|value| value.get("trigger"))
        .and_then(|value| value.as_str())
        .unwrap_or("manual");
    let before_tokens = metadata.and_then(|value| metadata_usize(value, "before_tokens"));
    let after_tokens = metadata.and_then(|value| metadata_usize(value, "after_tokens"));
    let archived_messages =
        metadata.and_then(|value| metadata_usize(value, "archived_message_count"));
    let preserved_messages =
        metadata.and_then(|value| metadata_usize(value, "preserved_message_count"));
    let duration_secs = metadata
        .and_then(|value| value.get("duration_secs"))
        .and_then(|value| value.as_f64());
    let verified = metadata
        .and_then(|value| value.get("verified"))
        .and_then(|value| value.as_bool());
    let verification_error = metadata
        .and_then(|value| value.get("verification_error"))
        .and_then(|value| value.as_str());

    let action_color = theme.colors.info.to_color();
    let mut result = match (before_tokens, after_tokens) {
        (Some(before), Some(after)) => {
            format!(
                "Success, {} -> {} tokens",
                format_compact_count(before),
                format_compact_count(after)
            )
        },
        _ => "Success".to_string(),
    };

    if let Some(count) = archived_messages {
        result.push_str(&format!(
            ", archived {} {}",
            count,
            if count == 1 { "message" } else { "messages" }
        ));
    }
    if let Some(count) = preserved_messages {
        result.push_str(&format!(
            ", preserved {} {}",
            count,
            if count == 1 { "message" } else { "messages" }
        ));
    }
    if let Some(verified) = verified {
        if verified {
            result.push_str(", verified");
        } else {
            result.push_str(", draft fallback");
        }
    }
    result = append_action_duration(result, duration_secs);

    let mut lines = vec![
        Line::from(vec![
            Span::styled("● ", Style::new().fg(action_color).bold()),
            Span::styled("Compact(", Style::new().fg(action_color).bold()),
            Span::styled(
                trigger.to_string(),
                Style::new().fg(theme.colors.text_secondary.to_color()),
            ),
            Span::styled(")", Style::new().fg(action_color).bold()),
        ]),
        Line::from(vec![
            Span::styled("  ⎿ ", Style::new().fg(action_color)),
            Span::styled(
                result,
                Style::new().fg(theme.colors.text_secondary.to_color()),
            ),
        ]),
    ];

    if let Some(error) = verification_error.filter(|error| !error.trim().is_empty()) {
        lines.push(Line::from(vec![
            Span::styled("    ", Style::new().fg(action_color)),
            Span::styled(
                format!("verification: {}", compact_inline_error(error, 180)),
                Style::new().fg(theme.colors.warning.to_color()),
            ),
        ]));
    }

    Some(lines)
}

fn metadata_usize(value: &serde_json::Value, key: &str) -> Option<usize> {
    value
        .get(key)?
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())
}

fn compact_inline_error(text: &str, max_chars: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let keep = max_chars.saturating_sub(3);
    let mut out: String = text.chars().take(keep).collect();
    out.push_str("...");
    out
}

/// Render actions in Claude Code style
fn render_actions(
    actions: &[ActionDisplay],
    lines: &mut Vec<Line>,
    theme: &Theme,
    viewport_width: usize,
) {
    for (action_idx, action) in actions.iter().enumerate() {
        if action_idx > 0 {
            lines.push(Line::from(""));
        }
        let action_color = match action.action_type.as_str() {
            "Write" | "Edit" => theme.colors.success.to_color(),
            "Delete" => theme.colors.warning.to_color(),
            _ => theme.colors.info.to_color(),
        };

        // Header: ● Type(target)
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
                // Result summary from details enum
                let result_msg = match &action.details {
                    ActionDetails::FileContent { line_count, .. } => {
                        let base = format!(
                            "Success, {} {} written",
                            line_count,
                            if *line_count == 1 { "line" } else { "lines" }
                        );
                        append_action_duration(base, action.duration_seconds)
                    },
                    ActionDetails::Diff { summary, .. } => summary.clone(),
                    ActionDetails::Preview { text, .. } => text.clone(),
                    ActionDetails::Simple => match action.action_type.as_str() {
                        "Delete" => append_action_duration(
                            format!("Success, deleted {}", action.target),
                            action.duration_seconds,
                        ),
                        _ => append_action_duration("Success".to_string(), action.duration_seconds),
                    },
                };

                for (idx, line) in result_msg.lines().enumerate() {
                    let prefix = if idx == 0 { "  ⎿ " } else { "    " };
                    lines.push(Line::from(vec![
                        Span::styled(prefix, Style::new().fg(action_color)),
                        Span::styled(
                            line.to_string(),
                            Style::new().fg(theme.colors.text_secondary.to_color()),
                        ),
                    ]));
                }

                // Write: syntax-highlighted file preview
                if let ActionDetails::FileContent {
                    content,
                    line_count,
                } = &action.details
                {
                    let preview_lines: Vec<&str> = content.lines().take(10).collect();
                    if !preview_lines.is_empty() {
                        lines.push(Line::from(vec![Span::styled(
                            "    ",
                            Style::new().fg(action_color),
                        )]));

                        let preview_content = preview_lines.join("\n");
                        let mut parsed = parse_markdown(&format!("```\n{}\n```", preview_content));
                        for parsed_line in parsed.iter_mut() {
                            let mut new_spans =
                                vec![Span::styled("    ", Style::new().fg(action_color))];
                            new_spans.append(&mut parsed_line.spans);
                            parsed_line.spans = new_spans;
                        }
                        lines.extend(parsed);

                        if *line_count > 10 {
                            lines.push(Line::from(vec![
                                Span::styled("    ", Style::new().fg(action_color)),
                                Span::styled(
                                    format!("... ({} more lines)", line_count - 10),
                                    Style::new()
                                        .fg(theme.colors.text_disabled.to_color())
                                        .italic(),
                                ),
                            ]));
                        }
                    }
                }

                // Edit: color-coded diff
                if let ActionDetails::Diff { diff, .. } = &action.details {
                    let diff_lines: Vec<&str> = diff.lines().collect();
                    let display_lines: Vec<&str> = diff_lines.iter().take(80).copied().collect();

                    if !display_lines.is_empty() {
                        let removed_bg = ratatui::style::Color::Rgb(60, 20, 20);
                        let added_bg = ratatui::style::Color::Rgb(20, 50, 20);

                        for diff_line in &display_lines {
                            // Delegate the producer-format awareness to
                            // `parse_diff_line`, which lives next to the
                            // marker constants and stays in lockstep with
                            // any future format change.
                            match parse_diff_line(diff_line) {
                                DiffLineKind::Removed => {
                                    let text = format!("    {}", diff_line);
                                    let padded =
                                        format!("{:<width$}", text, width = viewport_width);
                                    lines.push(Line::from(vec![Span::styled(
                                        padded,
                                        Style::new()
                                            .fg(theme.colors.error.to_color())
                                            .bg(removed_bg),
                                    )]));
                                },
                                DiffLineKind::Added => {
                                    let text = format!("    {}", diff_line);
                                    let padded =
                                        format!("{:<width$}", text, width = viewport_width);
                                    lines.push(Line::from(vec![Span::styled(
                                        padded,
                                        Style::new()
                                            .fg(theme.colors.success.to_color())
                                            .bg(added_bg),
                                    )]));
                                },
                                DiffLineKind::Context => {
                                    lines.push(Line::from(vec![
                                        Span::styled("    ", Style::new().fg(action_color)),
                                        Span::styled(
                                            diff_line.to_string(),
                                            Style::new().fg(theme.colors.text_secondary.to_color()),
                                        ),
                                    ]));
                                },
                            }
                        }

                        let remaining = diff_lines.len().saturating_sub(display_lines.len());
                        if remaining > 0 {
                            lines.push(Line::from(vec![
                                Span::styled("    ", Style::new().fg(action_color)),
                                Span::styled(
                                    format!("... ({} more lines)", remaining),
                                    Style::new()
                                        .fg(theme.colors.text_disabled.to_color())
                                        .italic(),
                                ),
                            ]));
                        }
                    }
                }
            },
            ActionResult::Error { error } => {
                let error =
                    append_action_duration(format!("Error: {}", error), action.duration_seconds);
                lines.push(Line::from(vec![
                    Span::styled("  ⎿ ", Style::new().fg(theme.colors.error.to_color())),
                    Span::styled(error, Style::new().fg(theme.colors.error.to_color())),
                ]));
            },
        }
    }
}

fn append_action_duration(mut text: String, duration_seconds: Option<f64>) -> String {
    if let Some(seconds) = duration_seconds {
        text.push_str(", took ");
        text.push_str(&format_action_duration(seconds));
    }
    text
}

fn format_action_duration(seconds: f64) -> String {
    if seconds < 1.0 {
        format!("{}ms", (seconds * 1000.0).round().max(1.0) as u64)
    } else if seconds < 10.0 {
        format!("{:.1}s", seconds)
    } else {
        format!("{}s", seconds.round() as u64)
    }
}

/// Wrap text with hanging indent support.
///
/// `width`, `first_line_indent`, and `continuation_indent` are all measured
/// in **display cells**, not bytes. Word lengths are also measured in cells
/// via `UnicodeWidthStr::width` so CJK / emoji wrap at the visual edge —
/// previously a CJK paragraph would wrap after ~1/3 of the line because
/// `word.len()` (bytes) is roughly 3× `word.width()` (cells) for 3-byte
/// codepoints.
fn wrap_text_with_indent(
    text: &str,
    width: usize,
    first_line_indent: usize,
    continuation_indent: usize,
) -> Vec<String> {
    let mut wrapped_lines = Vec::new();

    for (line_idx, line) in text.lines().enumerate() {
        if line.is_empty() {
            wrapped_lines.push(String::new());
            continue;
        }

        let current_indent = if line_idx == 0 {
            first_line_indent
        } else {
            continuation_indent
        };
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
        // Display-cell widths: indent is ASCII spaces (1 cell each), so
        // start fresh and let words contribute their own cell widths.
        let mut current_length = 0;

        for (word_idx, word) in words.iter().enumerate() {
            let word_width = word.width();

            if word_idx == 0 {
                // First word always fits on the line
                current_line.push_str(word);
                current_length = word_width;
            } else if current_length + 1 + word_width <= available_width {
                // Word fits on current line (the +1 accounts for the
                // separator space, which is 1 cell)
                current_line.push(' ');
                current_line.push_str(word);
                current_length += 1 + word_width;
            } else {
                // Word doesn't fit, start a new line
                wrapped_lines.push(current_line);
                current_line = String::with_capacity(width);
                current_line.push_str(&" ".repeat(continuation_indent));
                current_line.push_str(word);
                current_length = word_width;
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
fn wrap_styled_line(
    line: Line<'static>,
    width: usize,
    continuation_indent: usize,
) -> Vec<Line<'static>> {
    // Widths are counted in display cells (via `UnicodeWidthStr`), not
    // bytes. This makes CJK double-width chars and emoji wrap at the
    // correct visual column, and avoids over-wrapping multi-byte ASCII-
    // looking glyphs.
    let total_width: usize = line.spans.iter().map(|s| s.content.width()).sum();

    // If the line fits within width, return as-is
    if total_width <= width {
        return vec![line];
    }

    // Line needs wrapping - extract all text and styles
    let mut result_lines = Vec::new();
    let mut current_line_spans = Vec::new();
    let mut current_line_width = 0usize;
    let available_width = width.saturating_sub(continuation_indent);

    for span in line.spans.clone() {
        let span_text = span.content.to_string();
        let span_style = span.style;

        // Split span text by words
        let words: Vec<&str> = span_text.split_whitespace().collect();

        for (word_idx, word) in words.iter().enumerate() {
            let word_with_space = if word_idx > 0 || current_line_width > 0 {
                format!(" {}", word)
            } else {
                word.to_string()
            };

            let word_width = word_with_space.width();

            if current_line_width == 0 && result_lines.is_empty() {
                // First word of first line - no indent
                current_line_spans.push(Span::styled(word_with_space, span_style));
                current_line_width += word_width;
            } else if current_line_width + word_width <= available_width {
                // Word fits on current line
                current_line_spans.push(Span::styled(word_with_space, span_style));
                current_line_width += word_width;
            } else {
                // Word doesn't fit - finish current line and start new one
                result_lines.push(Line::from(current_line_spans));
                current_line_spans = vec![Span::raw(" ".repeat(continuation_indent))];
                current_line_spans.push(Span::styled(word.to_string(), span_style));
                current_line_width = word.width();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_at_cell_clamps_and_respects_cjk() {
        assert_eq!(byte_at_cell("hello", 0), 0);
        assert_eq!(byte_at_cell("hello", 3), 3);
        assert_eq!(byte_at_cell("hello", 99), 5); // clamp past end
        // "你好" = 2 chars, 3 bytes each, 2 cells each.
        assert_eq!(byte_at_cell("你好", 0), 0);
        assert_eq!(byte_at_cell("你好", 2), 3); // after first wide char
        // A cell index that lands mid-glyph keeps the glyph whole (rounds up).
        assert_eq!(byte_at_cell("你好", 1), 3);
    }

    #[test]
    fn slice_by_cells_extracts_display_range() {
        assert_eq!(slice_by_cells("hello world", 0, 5), "hello");
        assert_eq!(slice_by_cells("hello world", 6, 11), "world");
        assert_eq!(slice_by_cells("你好world", 2, 7), "好wor");
    }

    /// Build a ChatState whose last frame rendered `rows`, with a selection
    /// already mapped to content coords, so `selected_text` can be tested
    /// without a real terminal.
    fn state_with_rows(rows: &[&str], sel: ((usize, usize), (usize, usize))) -> ChatState {
        let mut st = ChatState::new();
        st.last_rendered_rows = rows.iter().map(|r| r.to_string()).collect();
        st.selection = Some(sel);
        st
    }

    #[test]
    fn selected_text_single_line() {
        let st = state_with_rows(&["> hello world"], ((0, 2), (0, 7)));
        assert_eq!(st.selected_text().as_deref(), Some("hello"));
    }

    #[test]
    fn selected_text_spans_multiple_rows() {
        let st = state_with_rows(&["> first line", "  second line"], ((0, 2), (1, 8)));
        // Copy-what-you-see: the start row is sliced from the click column,
        // and fully-covered continuation rows include their on-screen "  "
        // prefix (v1 behavior — the highlight matches exactly what's copied).
        assert_eq!(st.selected_text().as_deref(), Some("first line\n  second"));
    }

    #[test]
    fn selected_text_normalizes_reversed_drag() {
        // Dragging bottom-up / right-to-left yields the same text.
        let st = state_with_rows(&["> hello world"], ((0, 7), (0, 2)));
        assert_eq!(st.selected_text().as_deref(), Some("hello"));
    }

    #[test]
    fn selected_text_empty_selection_is_none() {
        // A plain click (anchor == cursor) selects nothing.
        let st = state_with_rows(&["> hello"], ((0, 3), (0, 3)));
        assert_eq!(st.selected_text(), None);
    }

    #[test]
    fn highlight_line_cells_splits_spans_on_selection() {
        let mut line = Line::from(vec![Span::raw("abcdef")]);
        highlight_line_cells(
            &mut line,
            2,
            4,
            Style::new().add_modifier(Modifier::REVERSED),
        );
        // Split into "ab" | "cd"(reversed) | "ef".
        let texts: Vec<String> = line.spans.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(texts, vec!["ab", "cd", "ef"]);
        assert!(
            line.spans[1]
                .style
                .add_modifier
                .contains(Modifier::REVERSED)
        );
        assert!(
            !line.spans[0]
                .style
                .add_modifier
                .contains(Modifier::REVERSED)
        );
    }

    #[test]
    fn context_checkpoint_renders_as_compact_event() {
        let mut msg = ChatMessage::user("full checkpoint summary hidden from the chat log");
        msg.kind = ChatMessageKind::ContextCheckpoint;
        msg.metadata = Some(serde_json::json!({
            "trigger": "manual",
            "before_tokens": 43_800,
            "after_tokens": 9_200,
            "archived_message_count": 18,
            "preserved_message_count": 4,
            "duration_secs": 2.4,
            "verified": true,
        }));

        let lines = render_context_checkpoint_event(&msg, &Theme::dark()).expect("event lines");
        let rendered = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("Compact(manual)"));
        assert!(rendered.contains("43.8k -> 9.2k tokens"));
        assert!(rendered.contains("archived 18 messages"));
        assert!(rendered.contains("preserved 4 messages"));
        assert!(rendered.contains("verified"));
        assert!(!rendered.contains("full checkpoint summary"));
    }

    #[test]
    fn context_checkpoint_renders_verification_fallback() {
        let mut msg = ChatMessage::user("full checkpoint summary hidden from the chat log");
        msg.kind = ChatMessageKind::ContextCheckpoint;
        msg.metadata = Some(serde_json::json!({
            "trigger": "auto_threshold",
            "before_tokens": 43_800,
            "after_tokens": 9_200,
            "archived_message_count": 18,
            "preserved_message_count": 4,
            "duration_secs": 2.4,
            "verified": false,
            "verification_error": "provider overloaded",
        }));

        let lines = render_context_checkpoint_event(&msg, &Theme::dark()).expect("event lines");
        let rendered = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("Compact(auto_threshold)"));
        assert!(rendered.contains("draft fallback"));
        assert!(rendered.contains("verification: provider overloaded"));
    }

    /// CJK characters are 3 bytes but 2 display cells each. The
    /// byte-length version of `wrap_styled_line` would incorrectly
    /// over-wrap such input. This test asserts the display-width
    /// version keeps CJK-only input on a single line when the display
    /// width fits, even when the byte length exceeds the width.
    #[test]
    fn wrap_styled_line_uses_display_width_for_cjk() {
        // "你好世界" is 4 CJK chars × 3 bytes = 12 bytes, × 2 display cells = 8 cells.
        // Target width of 10: byte-length would see 12 > 10 and wrap;
        // display-width sees 8 <= 10 and keeps it on one line.
        let line = Line::from(Span::raw("你好世界".to_string()));
        let wrapped = wrap_styled_line(line, 10, 2);
        assert_eq!(
            wrapped.len(),
            1,
            "CJK input fitting in display-width should NOT be wrapped; got {} lines",
            wrapped.len()
        );
    }

    /// Sanity: ASCII wrapping still works and produces >= 2 lines when
    /// the input exceeds the width.
    #[test]
    fn wrap_styled_line_ascii_wraps_when_too_long() {
        let line = Line::from(Span::raw(
            "the quick brown fox jumps over the lazy dog".to_string(),
        ));
        let wrapped = wrap_styled_line(line, 15, 2);
        assert!(
            wrapped.len() >= 2,
            "long ASCII input should wrap to multiple lines; got {}",
            wrapped.len()
        );
    }

    /// Counterpart to `wrap_styled_line_uses_display_width_for_cjk` for
    /// the plain-string wrapper used by user messages and thinking blocks.
    /// The byte-based version would wrap a 4-CJK paragraph after the second
    /// char (12 bytes > 10) even though it fits in 8 cells. Display-width
    /// version keeps it on one line.
    #[test]
    fn wrap_text_with_indent_uses_display_width_for_cjk() {
        // "你好世界" = 4 chars, 12 bytes, 8 display cells. Width 12 cells
        // with 0 indent: should fit on one line.
        let wrapped = wrap_text_with_indent("你好世界", 12, 0, 0);
        assert_eq!(
            wrapped.len(),
            1,
            "CJK paragraph fitting in display width should not wrap; got {} lines: {:?}",
            wrapped.len(),
            wrapped
        );
        assert_eq!(wrapped[0].trim_start(), "你好世界");
    }

    /// Mixed content: CJK + ASCII should still wrap correctly when the
    /// total exceeds available cells.
    #[test]
    fn wrap_text_with_indent_wraps_cjk_at_visual_edge() {
        // "你好 world 世界" = 2 + 1 + 5 + 1 + 2 = 11 cells without spaces,
        // with separators: 2 + 1 + 5 + 1 + 4 = 13 cells. Width 8 cells should
        // produce ≥ 2 lines.
        let wrapped = wrap_text_with_indent("你好 world 世界", 8, 0, 0);
        assert!(
            wrapped.len() >= 2,
            "mixed CJK+ASCII exceeding width should wrap; got {} lines: {:?}",
            wrapped.len(),
            wrapped
        );
    }
}
