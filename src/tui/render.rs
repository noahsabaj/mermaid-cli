use ratatui::{
    layout::{Constraint, Direction, Flex, Layout, Margin, Rect},
    Frame,
};
use std::sync::{LazyLock, Mutex};

use super::app::App;
use super::state::GenerationStatus;
use crate::tui::widgets::{ChatWidget, InputState, InputWidget, StatusLineWidget, StatusWidget};
use crate::utils::MutexExt;

/// Cache for layout calculations to improve performance
#[derive(Clone)]
struct LayoutCache {
    main_layout: Option<(u16, u16, Vec<Rect>)>, // (width, height, rects)
}

impl LayoutCache {
    fn new() -> Self {
        Self {
            main_layout: None,
        }
    }

    fn get_main_layout(&mut self, area: Rect, input_height: u16) -> Vec<Rect> {
        // Check if cached layout is still valid (cheap clone of Copy types)
        if let Some((w, h, ref rects)) = self.main_layout {
            if w == area.width && h == input_height {
                return rects.clone(); // Cheap: Vec of Copy types (5 Rects = ~40 bytes)
            }
        }

        // Clean layout with proper spacing (no overlap)
        // Layout: Chat Area | Status Line | Input Box | Status Bar
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .margin(0)
            .spacing(0)  // No negative spacing - prevents overlap
            .flex(Flex::Start)  // Align to top
            .constraints([
                Constraint::Min(10),    // Main content / Chat area (grows to fill)
                Constraint::Length(1),  // Status line (single line, only when generating)
                Constraint::Length(input_height),  // Dynamic input height (3-8 lines)
                Constraint::Length(2),  // Status bar (compact, 2 lines)
            ])
            .split(area);

        let layout_vec = layout.to_vec();
        self.main_layout = Some((area.width, input_height, layout_vec.clone()));
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

    // Use cached layout for better performance
    let chunks = {
        let mut cache = LAYOUT_CACHE.lock_mut_safe();
        cache.get_main_layout(frame.area(), input_height)
    };

    // Render chat area with horizontal padding using new ChatWidget
    let chat_area = chunks[0].inner(Margin {
        horizontal: 1,
        vertical: 0,
    });
    let chat_widget = ChatWidget {
        messages: &app.session_state.messages,
        is_generating: app.app_state.is_generating(),
        confirmation_state: app.operation_state.confirmation_state.as_ref(),
        pending_file_read: app.operation_state.pending_file_read,
        reading_file_status: app.operation_state.reading_file_status.as_deref(),
        theme: &app.ui_state.theme,
    };
    frame.render_stateful_widget(chat_widget, chat_area, &mut app.ui_state.chat_state);

    // Render status line when generating (shows progress: Thinking/Streaming, timer, token count)
    if app.app_state.is_generating() {
        let elapsed_secs = app
            .app_state
            .generation_start_time()
            .map(|start| start.elapsed().as_secs())
            .unwrap_or(0);

        let status_line_widget = StatusLineWidget {
            status: app.app_state.generation_status().unwrap_or(GenerationStatus::Idle),
            custom_status: app.status_state.custom_status.as_ref(),
            elapsed_secs,
            tokens_received: app.app_state.tokens_received().unwrap_or(0),
            theme: &app.ui_state.theme,
        };
        frame.render_widget(status_line_widget, chunks[1]);
    }

    // Render input area using new InputWidget (now at chunks[2])
    let input_widget = InputWidget {
        input: app.input.get(),
        showing_command_hints: app.input.get().starts_with(':'),
        theme: &app.ui_state.theme,
    };
    frame.render_stateful_widget(input_widget, chunks[2], &mut app.ui_state.input_state);

    // Set cursor position in input box (visible text cursor)
    let input_area = chunks[2];
    let inner_width = input_area.width as usize; // Full width now (no side borders)
    let (cursor_row, cursor_col) = InputState::calculate_cursor_position(
        app.input.get(),
        app.input.cursor_position,
        inner_width,
    );

    // Position cursor accounting for "> " prefix and top border
    // No left border anymore (spans full width), +1 for top border, +2 for "> " prefix
    frame.set_cursor_position((
        input_area.x + cursor_col + 2,
        input_area.y + 1 + cursor_row,
    ));

    // Render status bar using new StatusWidget (now at chunks[3])
    let status_widget = StatusWidget {
        operation_mode: app.operation_state.operation_mode,
        confirmation_pending: app.operation_state.confirmation_state.is_some(),
        theme: &app.ui_state.theme,
        working_dir: &app.working_dir,
        cumulative_tokens: app.session_state.cumulative_tokens,
        model_name: &app.model_state.model_name,
    };
    frame.render_widget(status_widget, chunks[3]);
}
