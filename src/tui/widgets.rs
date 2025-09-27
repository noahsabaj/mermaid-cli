use ratatui::{
    buffer::Buffer,
    layout::{Alignment, Constraint, Rect},
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, Cell, Row, StatefulWidget, Table, TableState, Widget},
};
use std::path::Path;

use crate::models::ProjectContext;
use crate::tui::app::{App, ConfirmationState};

/// Sidebar widget that displays file tree using a Table for better features
pub struct SidebarWidget<'a> {
    pub context: &'a ProjectContext,
    pub expanded: bool,
    pub working_dir: &'a str,
}

/// State for the sidebar widget
pub struct SidebarState {
    pub table_state: TableState,
    pub selected_file: usize,
}

impl SidebarState {
    pub fn new() -> Self {
        let mut state = Self {
            table_state: TableState::default(),
            selected_file: 0,
        };
        state.table_state.select(Some(0));
        state
    }

    pub fn next(&mut self, max: usize) {
        if self.selected_file < max.saturating_sub(1) {
            self.selected_file += 1;
            self.table_state.select(Some(self.selected_file));
        }
    }

    pub fn previous(&mut self) {
        if self.selected_file > 0 {
            self.selected_file -= 1;
            self.table_state.select(Some(self.selected_file));
        }
    }
}

impl<'a> StatefulWidget for SidebarWidget<'a> {
    type State = SidebarState;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        let mut rows = Vec::new();

        // Add project info
        if let Some(project_type) = &self.context.project_type {
            rows.push(Row::new(vec![
                Cell::from("[DIR]").style(Style::default().fg(Color::DarkGray)),
                Cell::from(format!("Project: {}", project_type))
                    .style(Style::default().fg(Color::Yellow)),
                Cell::from(""),
            ]));
        }

        // Add file count and token count
        rows.push(Row::new(vec![
            Cell::from("[INFO]").style(Style::default().fg(Color::DarkGray)),
            Cell::from(format!("Files: {}", self.context.files.len())),
            Cell::from(format!("Tokens: {}", self.context.token_count))
                .style(Style::default().fg(Color::Cyan)),
        ]));

        // Add separator
        rows.push(Row::new(vec![
            Cell::from(""),
            Cell::from(""),
            Cell::from(""),
        ]));

        // Add files with better formatting
        let max_files = if self.expanded {
            self.context.files.len()
        } else {
            20
        };

        for (path, _) in self.context.files.iter().take(max_files) {
            let path_obj = Path::new(path);
            let icon = if path.ends_with('/') {
                "[DIR]"
            } else {
                match path_obj.extension().and_then(|s| s.to_str()) {
                    Some("rs") => "[RS]",
                    Some("toml") => "[CFG]",
                    Some("md") => "[DOC]",
                    Some("js") | Some("ts") => "[JS]",
                    Some("py") => "[PY]",
                    _ => "[FILE]",
                }
            };

            let file_name = path_obj
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(path);

            let dir_path = path_obj.parent().and_then(|p| p.to_str()).unwrap_or("");

            rows.push(Row::new(vec![
                Cell::from(icon).style(Style::default().fg(Color::Blue)),
                Cell::from(file_name),
                Cell::from(dir_path).style(Style::default().fg(Color::DarkGray)),
            ]));
        }

        if !self.expanded && self.context.files.len() > 20 {
            rows.push(Row::new(vec![
                Cell::from("..."),
                Cell::from(format!("{} more files", self.context.files.len() - 20))
                    .style(Style::default().fg(Color::DarkGray)),
                Cell::from("Press 'e' to expand").style(Style::default().fg(Color::DarkGray)),
            ]));
        }

        // Create table with proper constraints
        let table = Table::new(
            rows,
            [
                Constraint::Length(6),      // Icon column
                Constraint::Min(20),        // File name
                Constraint::Percentage(40), // Directory path
            ],
        )
        .block(
            Block::default()
                .title(format!("Files [{}]", self.working_dir))
                .borders(Borders::RIGHT)
                .border_style(Style::default().fg(Color::DarkGray)),
        )
        .row_highlight_style(Style::default().bg(Color::Rgb(50, 50, 50)))
        .highlight_symbol("▶ ");

        // Render as stateful widget
        StatefulWidget::render(table, area, buf, &mut state.table_state);
    }
}

/// Confirmation dialog as a reusable StatefulWidget
pub struct ConfirmationDialog<'a> {
    pub confirmation: &'a ConfirmationState,
}

impl<'a> Widget for ConfirmationDialog<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // Calculate dialog size and position
        let width = area.width.min(80);
        let height = 12
            + if self.confirmation.file_info.is_some() {
                6
            } else {
                0
            };

        let dialog_area = Rect {
            x: area.x + (area.width.saturating_sub(width)) / 2,
            y: area.y + (area.height.saturating_sub(height)) / 2,
            width,
            height,
        };

        // Create the dialog block
        let block = Block::default()
            .title(format!(
                " Action: {} ",
                self.confirmation.action_description
            ))
            .title_alignment(Alignment::Center)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow))
            .style(Style::default().bg(Color::Rgb(20, 20, 20)));

        // Calculate inner area before rendering block
        let inner = block.inner(dialog_area);
        block.render(dialog_area, buf);
        let mut y = inner.y;

        // File info if available
        if let Some(ref info) = self.confirmation.file_info {
            // File path
            buf.set_string(
                inner.x + 2,
                y,
                format!("File: {}", info.path),
                Style::default().fg(Color::White),
            );
            y += 1;

            // Size and status
            let status = if info.exists {
                "Will overwrite"
            } else {
                "New file"
            };
            buf.set_string(
                inner.x + 2,
                y,
                format!("Size: {} bytes | {}", info.size, status),
                Style::default().fg(Color::Gray),
            );
            y += 1;

            // Language if detected
            if let Some(ref lang) = info.language {
                buf.set_string(
                    inner.x + 2,
                    y,
                    format!("Type: {}", lang),
                    Style::default().fg(Color::Gray),
                );
                y += 1;
            }

            // Preview
            if !self.confirmation.preview_lines.is_empty() {
                y += 1;
                buf.set_string(inner.x + 2, y, "Preview:", Style::default().fg(Color::Cyan));
                y += 1;

                for (i, line) in self.confirmation.preview_lines.iter().take(3).enumerate() {
                    let preview = if line.len() > width as usize - 6 {
                        format!("{}...", &line[..width as usize - 9])
                    } else {
                        line.clone()
                    };
                    buf.set_string(
                        inner.x + 4,
                        y + i as u16,
                        preview,
                        Style::default().fg(Color::Rgb(150, 150, 150)),
                    );
                }
            }
        }

        // Key shortcuts at bottom
        let shortcuts = if self.confirmation.allow_always {
            "[Alt+Y] Approve  [Alt+N] Skip  [Alt+A] Always  [Alt+P] Preview"
        } else {
            "[Alt+Y] Approve  [Alt+N] Skip  [Alt+P] Preview"
        };

        buf.set_string(
            inner.x + (inner.width.saturating_sub(shortcuts.len() as u16)) / 2,
            inner.y + inner.height - 2,
            shortcuts,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    }
}

/// Implementation of Widget trait for &App for better performance
impl Widget for &App {
    fn render(self, _area: Rect, _buf: &mut Buffer) {
        // This allows us to render App by reference instead of consuming it
        // Use this for frequently rendered components
        // In a full refactor, we'd implement the rendering logic here directly
    }
}
