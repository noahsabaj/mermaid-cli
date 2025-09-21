use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
    Frame,
};

use crate::tui::app::App;
use crate::models::MessageRole;

/// Render the main UI
pub fn render_ui(frame: &mut Frame, app: &App) {
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
    render_input(frame, chunks[2], app);

    // Render status bar
    render_status_bar(frame, chunks[3], app);
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

    let message_count = app.messages.len();
    for (idx, msg) in app.messages.iter().enumerate() {
        let _is_last_message = idx == message_count - 1;
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
            // Check if this message contains FILE_READ action
            let has_file_read = msg.content.contains("[FILE_READ:");

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

            // Only show "Response Complete" if no FILE_READ is pending
            if !has_file_read {
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
            } else {
                // Show reading status instead - it will be set by the model's preceding text
                if let Some(status) = &app.reading_file_status {
                    lines.push(Line::from(vec![
                        Span::styled(
                            "  📖 ",
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            status,
                            Style::default()
                                .fg(Color::Rgb(200, 200, 150))  // Warm yellow-gray
                                .add_modifier(Modifier::ITALIC),
                        ),
                    ]));
                }
            }

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

    // Show inline confirmation box if action pending
    if let Some(ref confirmation) = app.confirmation_state {
        let width = area.width.saturating_sub(4) as usize;

        // Top border
        lines.push(Line::from("╔".to_string() + &"═".repeat(width - 2) + "╗"));

        // Title line
        let title = format!("📝 Action: {}", confirmation.action_description);
        let title_len = title.len();
        lines.push(Line::from(vec![
            Span::raw("║ "),
            Span::styled(
                title,
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!("{}║", " ".repeat(width.saturating_sub(title_len + 3)))),
        ]));

        // File info if available
        if let Some(ref info) = confirmation.file_info {
            // Separator
            lines.push(Line::from("╠".to_string() + &"─".repeat(width - 2) + "╣"));

            // File path
            let path_line = format!("   File: {}", info.path);
            lines.push(Line::from(vec![
                Span::raw("║"),
                Span::raw(path_line.clone()),
                Span::raw(format!("{}║", " ".repeat(width.saturating_sub(path_line.len() + 1)))),
            ]));

            // Size and status
            let status = if info.exists { "Will overwrite" } else { "New file" };
            let size_line = format!("   Size: {} bytes | {}", info.size, status);
            lines.push(Line::from(vec![
                Span::raw("║"),
                Span::raw(size_line.clone()),
                Span::raw(format!("{}║", " ".repeat(width.saturating_sub(size_line.len() + 1)))),
            ]));

            // Language if detected
            if let Some(ref lang) = info.language {
                let lang_line = format!("   Type: {}", lang);
                lines.push(Line::from(vec![
                    Span::raw("║"),
                    Span::raw(lang_line.clone()),
                    Span::raw(format!("{}║", " ".repeat(width.saturating_sub(lang_line.len() + 1)))),
                ]));
            }

            // Preview if available
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
                    lines.push(Line::from(vec![
                        Span::raw("║"),
                        Span::styled(
                            truncated.clone(),
                            Style::default().fg(Color::Rgb(150, 150, 150)),
                        ),
                        Span::raw(format!("{}║", " ".repeat(width.saturating_sub(truncated.len() + 1)))),
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

        // Key shortcuts separator
        lines.push(Line::from("╠".to_string() + &"═".repeat(width - 2) + "╣"));

        // Key shortcuts
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
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!("{}║", " ".repeat(width.saturating_sub(shortcuts.len() + padding + 1)))),
        ]));

        // Bottom border
        lines.push(Line::from("╚".to_string() + &"═".repeat(width - 2) + "╝"));
        lines.push(Line::from(""));
    }

    // Show file reading status if active (when between FILE_READ and feedback)
    if app.pending_file_read && app.reading_file_status.is_some() && !app.is_generating {
        if let Some(status) = &app.reading_file_status {
            lines.push(Line::from(vec![
                Span::styled(
                    "  📖 ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    status,
                    Style::default()
                        .fg(Color::Rgb(200, 200, 150))  // Warm yellow-gray
                        .add_modifier(Modifier::ITALIC | Modifier::SLOW_BLINK),
                ),
            ]));
            lines.push(Line::from(""));
        }
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
fn render_input(frame: &mut Frame, area: Rect, app: &App) {
    // Check if we should show command hints
    let showing_command_hints = app.input.starts_with(':');

    // Adjust the input area height if showing command hints
    let (_hints_area, input_area) = if showing_command_hints {
        // Commands to show
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

        // Filter commands based on what user has typed
        let typed_command = app.input.trim_start_matches(':').to_lowercase();
        let filtered_commands: Vec<_> = if typed_command.is_empty() {
            commands.clone()
        } else {
            commands
                .into_iter()
                .filter(|(cmd, _)| {
                    cmd.trim_start_matches(':').to_lowercase().starts_with(&typed_command)
                })
                .collect()
        };

        // Calculate hints area height (max 6 lines to avoid taking too much space)
        let hints_height = (filtered_commands.len() as u16 + 2).min(8);

        // Split area for hints above input
        if area.height > hints_height {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(hints_height),
                    Constraint::Min(3),
                ])
                .split(area);

            // Render command hints in the hints area
            if !filtered_commands.is_empty() {
                let mut hint_lines = vec![
                    Line::from(vec![
                        Span::styled(
                            " Available Commands:",
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD),
                        ),
                    ]),
                ];

                for (cmd, desc) in filtered_commands.iter().take(6) {
                    hint_lines.push(Line::from(vec![
                        Span::styled(
                            format!("  {:<20}", cmd),
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            *desc,
                            Style::default().fg(Color::Gray),
                        ),
                    ]));
                }

                let hints_block = Paragraph::new(hint_lines)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(Color::DarkGray))
                            .title(" Commands (↑↓ to navigate, Enter to execute) "),
                    );

                frame.render_widget(hints_block, chunks[0]);
            }

            (Some(chunks[0]), chunks[1])
        } else {
            (None, area)
        }
    } else {
        (None, area)
    };

    // Render the input box
    let input_style = Style::default().fg(Color::White);
    let title = if showing_command_hints {
        " Enter Command "
    } else {
        " Message (Esc to stop/clear • Type :help for commands) "
    };
    let input_text = app.input.clone();

    let input = Paragraph::new(input_text)
        .style(input_style)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(
                    if showing_command_hints {
                        Color::Yellow
                    } else {
                        Color::DarkGray
                    }
                ))
                .title(title),
        );

    frame.render_widget(input, input_area);

    // Always show cursor
    {
        // Calculate cursor position based on input length
        let cursor_offset = app.input.len();

        // Position cursor at the end of the input text
        let cursor_x = input_area.x + 1 + cursor_offset as u16;
        let cursor_y = input_area.y + 1; // +1 to account for the border

        // Ensure cursor doesn't go beyond the input area bounds
        let cursor_x = cursor_x.min(input_area.x + input_area.width.saturating_sub(2));

        // Set the cursor position
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}

/// Render the status bar
fn render_status_bar(frame: &mut Frame, area: Rect, app: &App) {
    // We're always in "chat" mode now
    let mode_str = "CHAT";
    let mode_color = Color::Green;

    let status_text = if app.confirmation_state.is_some() {
        "⚠️ Action pending: Alt+Y to approve, Alt+N to skip, Alt+A for always".to_string()
    } else if let Some(status) = &app.status_message {
        status.clone()
    } else if app.is_generating {
        "Generating response...".to_string()
    } else {
        "Ready".to_string()
    };

    // Build status line - show only OperationMode now
    let mut spans = vec![
        // Always in chat mode
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