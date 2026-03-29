use ratatui::{
    layout::{Constraint, Direction, Flex, Layout, Margin, Rect},
    Frame,
};
use std::sync::{LazyLock, Mutex};

use super::app::App;
use super::state::GenerationStatus;
use crate::tui::widgets::{AttachmentWidget, ChatWidget, InputState, InputWidget, StatusLineWidget, StatusWidget};
use crate::utils::MutexExt;

/// Cache for layout calculations to improve performance
#[derive(Clone)]
struct LayoutCache {
    main_layout: Option<(u16, u16, u16, u16, Vec<Rect>)>, // (width, input_height, status_height, attachment_height, rects)
}

impl LayoutCache {
    fn new() -> Self {
        Self {
            main_layout: None,
        }
    }

    fn get_main_layout(&mut self, area: Rect, input_height: u16, status_line_height: u16, attachment_height: u16) -> Vec<Rect> {
        // Check if cached layout is still valid (cheap clone of Copy types)
        if let Some((w, ih, sh, ah, ref rects)) = self.main_layout
            && w == area.width && ih == input_height && sh == status_line_height && ah == attachment_height {
                return rects.clone();
            }

        // Clean layout with proper spacing (no overlap)
        // Layout: Chat Area | Status Line | Attachments | Input Box | Status Bar
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .margin(0)
            .spacing(0)
            .flex(Flex::Start)
            .constraints([
                Constraint::Min(10),                      // Chat area (grows to fill)
                Constraint::Length(status_line_height),    // Status line
                Constraint::Length(attachment_height),     // Attachment indicators (0 when empty)
                Constraint::Length(input_height),          // Input box
                Constraint::Length(2),                     // Status bar
            ])
            .split(area);

        let layout_vec = layout.to_vec();
        self.main_layout = Some((area.width, input_height, status_line_height, attachment_height, layout_vec.clone()));
        layout_vec
    }
}

// Global layout cache
static LAYOUT_CACHE: LazyLock<Mutex<LayoutCache>> = LazyLock::new(|| Mutex::new(LayoutCache::new()));

/// Render the main UI
pub fn render_ui(frame: &mut Frame, app: &mut App) {
    // Update terminal window title
    if let Some(ref title) = app.session_state.conversation_title {
        app.set_terminal_title(title);
    } else {
        // Default title when no conversation title yet
        app.set_terminal_title(&format!("mermaid - {}", app.working_dir));
    }

    // Calculate input area height based on content
    let terminal_width = frame.area().width.saturating_sub(4) as usize; // Account for borders
    let input_lines = if app.input.is_empty() {
        1
    } else {
        // Calculate how many lines the input will take
        let mut lines = 1;
        let mut current_line_length = 0;
        for ch in app.input.get().chars() {
            if ch == '\n' || current_line_length >= terminal_width {
                lines += 1;
                current_line_length = if ch == '\n' { 0 } else { 1 };
            } else {
                current_line_length += 1;
            }
        }
        lines.min(5) // Cap at 5 lines max
    };
    let input_height = (input_lines + 2) as u16; // +2 for borders

    // Calculate status line height: 1 + number of queued messages
    let queued_count = app.operation_state.queued_message_count();
    let status_line_height = (1 + queued_count).min(6) as u16; // Cap at 6 lines

    // Attachment area: 1 line when attachments present (all images on one line), 0 when empty
    let attachment_height = if app.attachment_state.is_empty() { 0 } else { 1 };

    // Use cached layout for better performance
    let chunks = {
        let mut cache = LAYOUT_CACHE.lock_mut_safe();
        cache.get_main_layout(frame.area(), input_height, status_line_height, attachment_height)
    };

    // Render chat area with horizontal padding using new ChatWidget
    let chat_area = chunks[0].inner(Margin {
        horizontal: 1,
        vertical: 0,
    });
    let chat_widget = ChatWidget {
        messages: &app.session_state.messages,
        is_generating: app.app_state.is_generating(),
        pending_file_read: app.operation_state.pending_file_read,
        reading_file_status: app.operation_state.reading_file_status.as_deref(),
        theme: &app.ui_state.theme,
        markdown_cache: &mut app.session_state.markdown_cache,
    };
    frame.render_stateful_widget(chat_widget, chat_area, &mut app.ui_state.chat_state);

    // Render status line when generating (shows progress: Thinking/Streaming, timer, token count)
    if app.app_state.is_generating() {
        let elapsed_secs = app
            .app_state
            .generation_start_time()
            .map(|start| start.elapsed().as_secs())
            .unwrap_or(0);

        // Estimate tokens during streaming: ~4 chars per token
        // Use actual count if available (set at end), otherwise estimate from response length
        let actual_tokens = app.app_state.tokens_received().unwrap_or(0);
        let (tokens_display, tokens_estimated) = if actual_tokens == 0 && !app.current_response.is_empty() {
            // Estimate: ~4 characters per token (rough approximation)
            (app.current_response.len() / 4, true)
        } else {
            (actual_tokens, false)
        };

        let status_line_widget = StatusLineWidget {
            status: app.app_state.generation_status().unwrap_or(GenerationStatus::Idle),
            custom_status: app.status_state.custom_status.as_ref(),
            elapsed_secs,
            tokens_received: tokens_display,
            tokens_estimated,
            theme: &app.ui_state.theme,
            queued_messages: app.operation_state.get_queued_messages(),
        };
        frame.render_widget(status_line_widget, chunks[1]);
    }

    // Render attachment indicators above input (chunks[2])
    if !app.attachment_state.is_empty() {
        app.ui_state.attachment_area_y = Some(chunks[2].y);
        let attachment_widget = AttachmentWidget {
            attachments: &app.attachment_state.attachments,
            theme: &app.ui_state.theme,
            focused: app.ui_state.attachment_focused,
            selected: app.ui_state.selected_attachment,
        };
        frame.render_widget(attachment_widget, chunks[2]);
    } else {
        app.ui_state.attachment_area_y = None;
    }

    // Render input area (chunks[3])
    let input_widget = InputWidget {
        input: app.input.get(),
        showing_command_hints: app.input.get().starts_with(':'),
        theme: &app.ui_state.theme,
        thinking_enabled: app.model_state.thinking_enabled,
    };
    frame.render_stateful_widget(input_widget, chunks[3], &mut app.ui_state.input_state);

    if app.ui_state.attachment_focused {
        // When attachment area is focused, hide cursor (selection highlight shows focus)
        // Place cursor offscreen by not setting it
    } else {
        // Set cursor position in input box (visible text cursor)
        let input_area = chunks[3];
        let content_width = input_area.width.saturating_sub(2) as usize;
        let (cursor_row, cursor_col) = InputState::calculate_cursor_position(
            app.input.get(),
            app.input.cursor_position,
            content_width,
        );

        // cursor_col is relative to content start (after "> " or "  " prefix)
        // +2 for the prefix, +1 for top border (no left border)
        frame.set_cursor_position((
            input_area.x + cursor_col + 2,
            input_area.y + 1 + cursor_row,
        ));
    }

    // Render status bar (chunks[4])
    let status_widget = StatusWidget {
        theme: &app.ui_state.theme,
        working_dir: &app.working_dir,
        cumulative_tokens: app.session_state.cumulative_tokens,
        model_name: &app.model_state.model_name,
        thinking_enabled: app.model_state.thinking_enabled,
    };
    frame.render_widget(status_widget, chunks[4]);
}
