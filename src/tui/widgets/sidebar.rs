use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Rect},
    style::{Color, Style},
    widgets::{Block, Borders, Cell, Row, StatefulWidget, Table, TableState},
};
use std::path::Path;

use crate::models::ProjectContext;
use crate::tui::theme::Theme;

/// Sidebar widget that displays file tree using a Table
pub struct SidebarWidget<'a> {
    pub context: &'a ProjectContext,
    pub expanded: bool,
    pub working_dir: &'a str,
    pub theme: &'a Theme,
}

/// State for the sidebar widget
#[derive(Debug, Clone)]
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

impl Default for SidebarState {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> StatefulWidget for SidebarWidget<'a> {
    type State = SidebarState;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        let mut rows = Vec::new();

        // Add project info
        if let Some(project_type) = &self.context.project_type {
            rows.push(Row::new(vec![
                Cell::from("[DIR]").style(Style::new().fg(Color::DarkGray)),
                Cell::from(format!("Project: {}", project_type))
                    .style(Style::new().fg(self.theme.colors.text_highlight.to_color())),
                Cell::from(""),
            ]));
        }

        // Add file count and token count
        rows.push(Row::new(vec![
            Cell::from("[INFO]").style(Style::new().fg(Color::DarkGray)),
            Cell::from(format!("Files: {}", self.context.files.len())),
            Cell::from(format!("Tokens: {}", self.context.token_count))
                .style(Style::new().fg(self.theme.colors.info.to_color())),
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
                Cell::from(icon).style(Style::new().fg(self.theme.colors.info.to_color())),
                Cell::from(file_name),
                Cell::from(dir_path).style(Style::new().fg(Color::DarkGray)),
            ]));
        }

        if !self.expanded && self.context.files.len() > 20 {
            rows.push(Row::new(vec![
                Cell::from("..."),
                Cell::from(format!("{} more files", self.context.files.len() - 20))
                    .style(Style::new().fg(Color::DarkGray)),
                Cell::from("Press 'e' to expand").style(Style::new().fg(Color::DarkGray)),
            ]));
        }

        // Create table with proper constraints
        let table = Table::new(
            rows,
            [
                Constraint::Length(6),
                Constraint::Min(20),
                Constraint::Percentage(40),
            ],
        )
        .block(
            Block::default()
                .title(format!("Files [{}]", self.working_dir))
                .borders(Borders::RIGHT)
                .border_style(Style::new().fg(self.theme.colors.border.to_color())),
        )
        .row_highlight_style(Style::new().bg(Color::Rgb(50, 50, 50)))
        .highlight_symbol("▶ ");

        StatefulWidget::render(table, area, buf, &mut state.table_state);
    }
}
