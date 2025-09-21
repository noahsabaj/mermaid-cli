use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
    Frame,
};

use crate::tui::app::{App, AppState, MessageRole};

/// Render the main UI
pub fn render_ui(frame: &mut Frame, app: &App, app_state: &AppState) {
    // Create main layout
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(0)
        .constraints(
            [
                Constraint::Length(3),  // Header
                Constraint::Min(10),    // Main content
                Constraint::Length(3),  // Input
                Constraint::Length(1),  // Status bar
            ]
            .as_ref(),
        )
        .split(frame.area());

    // Render header
    render_header(frame, chunks[0], app);

    // Split main content area
    let content_chunks = if app.show_sidebar {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(25), Constraint::Percentage(75)].as_ref())
            .split(chunks[1])
    } else {
        std::rc::Rc::new([Rect::default(), chunks[1]])
    };

    // Render sidebar if visible
    if app.show_sidebar {
        render_sidebar(frame, content_chunks[0], app);
    }

    // Render chat area
    render_chat(frame, content_chunks[1], app);

    // Render input area
    render_input(frame, chunks[2], app, app_state);

    // Render status bar
    render_status_bar(frame, chunks[3], app, app_state);
}

/// Render the header
fn render_header(frame: &mut Frame, area: Rect, app: &App) {
    let header_text = vec![
        Line::from(vec![
            Span::styled("🧜‍♀️ ", Style::default().fg(Color::Cyan)),
            Span::styled(
                "Mermaid",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" | Model: "),
            Span::styled(
                &app.model_name,
                Style::default().fg(Color::Green),
            ),
            Span::raw(" | "),
            Span::styled(
                &app.working_dir,
                Style::default().fg(Color::Gray),
            ),
        ]),
    ];

    let header = Paragraph::new(header_text)
        .block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(Style::default().fg(Color::DarkGray)),
        )
        .alignment(Alignment::Center);

    frame.render_widget(header, area);
}

/// Render the sidebar with file tree
fn render_sidebar(frame: &mut Frame, area: Rect, app: &App) {
    let mut items = Vec::new();

    // Add project info
    if let Some(project_type) = &app.context.project_type {
        items.push(ListItem::new(Line::from(vec![
            Span::raw("📁 "),
            Span::styled(
                format!("Project: {}", project_type),
                Style::default().fg(Color::Yellow),
            ),
        ])));
    }

    items.push(ListItem::new(Line::from(vec![
        Span::raw("📊 "),
        Span::raw(format!("Files: {}", app.context.files.len())),
    ])));

    items.push(ListItem::new(Line::from(vec![
        Span::raw("🔤 "),
        Span::raw(format!("Tokens: {}", app.context.token_count)),
    ])));

    items.push(ListItem::new(""));

    // Add file list (truncated or expanded)
    let max_files = if app.sidebar_expanded {
        app.context.files.len()
    } else {
        20
    };

    for (path, _) in app.context.files.iter().take(max_files) {
        let icon = if path.ends_with('/') {
            "📁"
        } else {
            "📄"
        };
        items.push(ListItem::new(Line::from(vec![
            Span::raw(format!("{} ", icon)),
            Span::raw(path),
        ])));
    }

    if !app.sidebar_expanded && app.context.files.len() > 20 {
        items.push(ListItem::new(Line::from(vec![
            Span::styled(
                format!("... and {} more (press 'e' to expand)", app.context.files.len() - 20),
                Style::default().fg(Color::DarkGray),
            ),
        ])));
    }

    let list = List::new(items)
        .block(
            Block::default()
                .title(format!("Files [{}] ", app.working_dir))
                .borders(Borders::RIGHT)
                .border_style(Style::default().fg(Color::DarkGray)),
        )
        .style(Style::default().fg(Color::White));

    frame.render_widget(list, area);
}

/// Render the chat area
fn render_chat(frame: &mut Frame, area: Rect, app: &App) {
    let mut lines = Vec::new();

    for msg in &app.messages {
        // Add role indicator
        let (role_span, role_color) = match msg.role {
            MessageRole::User => ("You", Color::Blue),
            MessageRole::Assistant => ("Mermaid", Color::Green),
            MessageRole::System => ("System", Color::Yellow),
        };

        lines.push(Line::from(vec![
            Span::styled(
                format!("[{}] ", role_span),
                Style::default()
                    .fg(role_color)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));

        // Add message content (split by lines)
        for line in msg.content.lines() {
            lines.push(Line::from(line.to_string()));
        }

        // Add completion indicator for assistant messages
        if matches!(msg.role, MessageRole::Assistant) {
            // Create a separator line that spans most of the width (accounting for borders)
            let width = area.width.saturating_sub(4) as usize; // Subtract for borders and padding
            let separator = "─".repeat(width);

            lines.push(Line::from(vec![
                Span::styled(
                    separator,
                    Style::default()
                        .fg(Color::Rgb(100, 100, 100))  // More visible gray
                ),
            ]));

            // Add completion message with checkmark
            lines.push(Line::from(vec![
                Span::styled(
                    "  ✓ ",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    "Response Complete",
                    Style::default()
                        .fg(Color::Rgb(150, 150, 150))  // Light gray text
                        .add_modifier(Modifier::ITALIC),
                ),
            ]));

            // Add another separator for better visual separation
            let separator2 = "─".repeat(width);
            lines.push(Line::from(vec![
                Span::styled(
                    separator2,
                    Style::default()
                        .fg(Color::Rgb(100, 100, 100)),
                ),
            ]));
        }

        lines.push(Line::from("")); // Empty line between messages
    }

    // Add current response if generating
    if app.is_generating && !app.current_response.is_empty() {
        lines.push(Line::from(vec![
            Span::styled(
                "[Mermaid] ",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));

        for line in app.current_response.lines() {
            lines.push(Line::from(line.to_string()));
        }

        // Add typing indicator
        lines.push(Line::from(vec![
            Span::styled("▋", Style::default().fg(Color::Green).add_modifier(Modifier::SLOW_BLINK)),
        ]));
    }

    // Use operation mode color for border to provide visual feedback
    let border_color = app.operation_mode.color();
    let title = format!("Chat [{}]", app.operation_mode.display_name());

    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color)),
        )
        .wrap(Wrap { trim: false })
        .scroll((app.scroll_offset, 0));

    frame.render_widget(paragraph, area);
}

/// Render the input area
fn render_input(frame: &mut Frame, area: Rect, app: &App, app_state: &AppState) {
    let (input_style, title) = match app_state {
        AppState::Insert => (
            Style::default().fg(Color::White),
            " Message (Esc to cancel) ",
        ),
        AppState::Command => (
            Style::default().fg(Color::Yellow),
            " Command ",
        ),
        _ => (Style::default().fg(Color::DarkGray), " Press 'i' to type "),
    };

    let input_text = match app_state {
        AppState::Command => format!(":{}", app.input),
        _ => app.input.clone(),
    };

    let input = Paragraph::new(input_text)
        .style(input_style)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray))
                .title(title),
        );

    frame.render_widget(input, area);

    // Show cursor in input mode
    if matches!(app_state, AppState::Insert | AppState::Command) {
        // Calculate cursor position based on input length
        let cursor_offset = match app_state {
            AppState::Command => app.input.len() + 1, // +1 for the ':' prefix
            _ => app.input.len(),
        };

        // Position cursor at the end of the input text
        let cursor_x = area.x + 1 + cursor_offset as u16;
        let cursor_y = area.y + 1; // +1 to account for the border

        // Ensure cursor doesn't go beyond the input area bounds
        let cursor_x = cursor_x.min(area.x + area.width.saturating_sub(2));

        // Set the cursor position
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}

/// Render the status bar
fn render_status_bar(frame: &mut Frame, area: Rect, app: &App, app_state: &AppState) {
    let mode_str = match app_state {
        AppState::Normal => "NORMAL",
        AppState::Insert => "INSERT",
        AppState::Command => "COMMAND",
        AppState::FileSelect => "FILES",
    };

    let mode_color = match app_state {
        AppState::Normal => Color::Blue,
        AppState::Insert => Color::Green,
        AppState::Command => Color::Yellow,
        AppState::FileSelect => Color::Magenta,
    };

    let status_text = if app.pending_action.is_some() {
        "⚠️ Action pending confirmation: Press 'y' to confirm, 'n' to skip".to_string()
    } else if let Some(status) = &app.status_message {
        status.clone()
    } else if app.is_generating {
        "Generating response...".to_string()
    } else {
        "Ready".to_string()
    };

    // Build status line - show both AppState and OperationMode
    let mut spans = vec![
        // AppState mode (Normal/Insert/Command)
        Span::styled(
            format!(" {} ", mode_str),
            Style::default()
                .bg(mode_color)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" | "),
        // OperationMode indicator
        Span::styled(
            format!(" {} ", app.operation_mode.display_name()),
            Style::default()
                .bg(app.operation_mode.color())
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" | "),
    ];

    // Add warning message if in dangerous mode
    let warning_level = app.operation_mode.warning_level();
    if let Some(warning) = warning_level.message() {
        spans.push(Span::styled(
            warning,
            Style::default()
                .fg(warning_level.color())
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(" | "));
    }
    spans.push(Span::raw(status_text));
    spans.push(Span::raw(" | "));
    spans.push(Span::styled(
        "Shift+Tab: cycle modes",
        Style::default().fg(Color::DarkGray),
    ));
    spans.push(Span::raw(" | "));
    spans.push(Span::styled(
        "Ctrl+C: quit",
        Style::default().fg(Color::DarkGray),
    ));

    // Create the status line and ensure it fills the entire width
    let status_line = Line::from(spans);

    // Use Block to ensure the entire area is cleared before rendering
    let status_bar = Paragraph::new(vec![status_line])
        .style(Style::default().bg(Color::Black))
        .block(Block::default()); // This ensures the entire area is cleared

    frame.render_widget(status_bar, area);
}