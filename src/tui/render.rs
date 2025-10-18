use once_cell::sync::Lazy;
use ratatui::{
    layout::{Constraint, Direction, Flex, Layout, Margin, Rect},
    Frame,
};
use std::sync::Mutex;

use crate::diagnostics::{render_diagnostics_panel, DiagnosticsMode};
use crate::tui::app::App;
use crate::tui::widgets::{ChatWidget, HeaderWidget, InputState, InputWidget, SidebarWidget, StatusLineWidget, StatusWidget};
use crate::utils::MutexExt;

/// Cache for layout calculations to improve performance
#[derive(Clone)]
struct LayoutCache {
    main_layout: Option<(u16, u16, Vec<Rect>)>, // (width, height, rects)
    content_layout: Option<(bool, Rect, Vec<Rect>)>, // (show_sidebar, area, rects)
}

impl LayoutCache {
    fn new() -> Self {
        Self {
            main_layout: None,
            content_layout: None,
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
        // Layout: Header | Chat Area | Status Line | Input Box | Status Bar
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .margin(0)
            .spacing(0)  // No negative spacing - prevents overlap
            .flex(Flex::Start)  // Align to top
            .constraints([
                Constraint::Length(1),  // Header (minimal, single line)
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

    fn get_content_layout(&mut self, show_sidebar: bool, area: Rect) -> Vec<Rect> {
        // Check if cached layout is still valid (cheap clone of Copy types)
        if let Some((sidebar, cached_area, ref rects)) = self.content_layout {
            if sidebar == show_sidebar && cached_area == area {
                return rects.clone(); // Cheap: Vec of Copy types (2 Rects = ~16 bytes)
            }
        }

        let layout = if show_sidebar {
            Layout::default()
                .direction(Direction::Horizontal)
                .spacing(1)  // Proper spacing between sidebar and main content
                .flex(Flex::Start)  // Align to left
                .constraints([Constraint::Min(20), Constraint::Fill(1)])
                .split(area)
                .to_vec()
        } else {
            vec![Rect::default(), area]
        };

        self.content_layout = Some((show_sidebar, area, layout.clone()));
        layout
    }
}

// Global layout cache
static LAYOUT_CACHE: Lazy<Mutex<LayoutCache>> = Lazy::new(|| Mutex::new(LayoutCache::new()));

/// Render the main UI
pub fn render_ui(frame: &mut Frame, app: &mut App) {
    // Calculate input area height based on content
    let terminal_width = frame.area().width.saturating_sub(4) as usize; // Account for borders
    let input_lines = if app.input.is_empty() {
        1
    } else {
        // Calculate how many lines the input will take
        let mut lines = 1;
        let mut current_line_length = 0;
        for ch in app.input.chars() {
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

    // Render header using new HeaderWidget
    let header = HeaderWidget {
        model_name: &app.model_name,
        working_dir: &app.working_dir,
        theme: &app.theme,
    };
    frame.render_widget(header, chunks[0]);

    // Use cached content layout
    let content_chunks = {
        let mut cache = LAYOUT_CACHE.lock_mut_safe();
        cache.get_content_layout(app.show_sidebar, chunks[1])
    };

    // Render sidebar if visible using new SidebarWidget
    if app.show_sidebar {
        let sidebar = SidebarWidget {
            context: &app.context,
            expanded: app.sidebar_expanded,
            working_dir: &app.working_dir,
            theme: &app.theme,
        };
        frame.render_stateful_widget(sidebar, content_chunks[0], &mut app.sidebar_state);
    }

    // Render chat area with horizontal padding using new ChatWidget
    let chat_area = content_chunks[1].inner(Margin {
        horizontal: 1,
        vertical: 0,
    });
    let chat_widget = ChatWidget {
        messages: &app.messages,
        current_response: &app.current_response,
        is_generating: app.is_generating,
        confirmation_state: app.confirmation_state.as_ref(),
        pending_file_read: app.pending_file_read,
        reading_file_status: app.reading_file_status.as_deref(),
        operation_mode: app.operation_mode,
        theme: &app.theme,
    };
    frame.render_stateful_widget(chat_widget, chat_area, &mut app.chat_state);

    // Render status line when generating (shows progress: Thinking/Streaming, timer, token count)
    if app.is_generating {
        let elapsed_secs = app
            .generation_start_time
            .map(|start| start.elapsed().as_secs())
            .unwrap_or(0);

        let status_line_widget = StatusLineWidget {
            status: app.generation_status,
            custom_status: app.custom_status.as_ref(),
            elapsed_secs,
            tokens_received: app.tokens_received,
            theme: &app.theme,
        };
        frame.render_widget(status_line_widget, chunks[2]);
    }

    // Render input area using new InputWidget (now at chunks[3])
    let input_widget = InputWidget {
        input: &app.input,
        showing_command_hints: app.input.starts_with(':'),
        theme: &app.theme,
    };
    frame.render_stateful_widget(input_widget, chunks[3], &mut app.input_state);

    // Set cursor position in input box (visible text cursor)
    let input_area = chunks[3];
    let inner_width = input_area.width.saturating_sub(2) as usize; // -2 for left+right borders
    let (cursor_row, cursor_col) = InputState::calculate_cursor_position(
        &app.input,
        app.cursor_position,
        inner_width,
    );

    // Position cursor accounting for borders
    // +1 for left border, +1 for top border
    frame.set_cursor_position((
        input_area.x + 1 + cursor_col,
        input_area.y + 1 + cursor_row,
    ));

    // Render status bar using new StatusWidget (now at chunks[4])
    let status_widget = StatusWidget {
        operation_mode: app.operation_mode,
        status_message: app.status_message.as_deref(),
        confirmation_pending: app.confirmation_state.is_some(),
        is_generating: app.is_generating,
        theme: &app.theme,
    };
    frame.render_widget(status_widget, chunks[4]);

    // Render diagnostics panel if in detailed mode
    if app.diagnostics_mode == DiagnosticsMode::Detailed {
        if let Some(ref stats) = app.hardware_stats {
            render_diagnostics_panel(frame, frame.area(), stats);
        }
    }
}
