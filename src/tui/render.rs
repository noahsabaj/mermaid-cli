use ratatui::{Frame, layout::Margin};

use super::app::App;
use super::state::GenerationStatus;
use crate::tui::widgets::{
    AttachmentWidget, ChatWidget, InputState, InputWidget, StatusLineWidget, StatusWidget,
};

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
    let attachment_height = if app.attachment_state.is_empty() {
        0
    } else {
        1
    };

    // Use cached layout for better performance (local to UIState, no global mutex)
    let chunks = app.ui_state.layout_cache.get_main_layout(
        frame.area(),
        input_height,
        status_line_height,
        attachment_height,
    );

    // Render chat area with horizontal padding using new ChatWidget
    let chat_area = chunks[0].inner(Margin {
        horizontal: 1,
        vertical: 0,
    });
    // Snapshot subagent progress (locks mutex briefly, clones max 10 small entries)
    let active_subagents = app.operation_state.snapshot_subagent_progress();

    let chat_widget = ChatWidget {
        messages: &app.session_state.messages,
        theme: &app.ui_state.theme,
        markdown_cache: &mut app.ui_state.markdown_cache,
        active_subagents,
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
        let (tokens_display, tokens_estimated) =
            if actual_tokens == 0 && app.response_len() > 0 {
                // Estimate: ~4 characters per token (rough approximation)
                (app.response_len() / 4, true)
            } else {
                (actual_tokens, false)
            };

        let status_line_widget = StatusLineWidget {
            status: app
                .app_state
                .generation_status()
                .unwrap_or(GenerationStatus::Idle),
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
        frame.set_cursor_position((input_area.x + cursor_col + 2, input_area.y + 1 + cursor_row));
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
