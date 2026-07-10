//! @-mention file picker widget — renders the fuzzy-ranked file matches for
//! the active `@`-token with the selected row highlighted. Visible whenever
//! `UiState::file_picker_open()` (an @-token under the cursor, not
//! dismissed, not a slash command); replaces the bottom status bar while
//! open, same screen region as the slash palette.
//!
//! Keyboard handling lives in the reducer's file-picker interception block.
//! This widget is purely presentational — it consumes pre-ranked matches
//! and a selection index.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Widget},
};

use crate::render::theme::Theme;

/// Visible-row window, matching the slash palette's geometry.
const MAX_VISIBLE_ROWS: usize = 8;

pub struct FilePickerWidget<'a> {
    pub theme: &'a Theme,
    /// Fuzzy-ranked matches, best first (already capped by the reducer).
    pub matches: &'a [String],
    /// Index into `matches` of the highlighted row; clamped defensively.
    pub selected_index: usize,
    /// First walk still in flight and no cached list yet.
    pub loading: bool,
}

impl<'a> Widget for FilePickerWidget<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // Same scroll-window math as the slash palette: selection anchors at
        // the bottom row once it passes the visible band.
        let total = self.matches.len();
        let selected = self.selected_index.min(total.saturating_sub(1));
        let scroll_offset = if selected >= MAX_VISIBLE_ROWS {
            selected + 1 - MAX_VISIBLE_ROWS
        } else {
            0
        };
        let visible_end = (scroll_offset + MAX_VISIBLE_ROWS).min(total);

        let title = if total > MAX_VISIBLE_ROWS {
            format!(
                " Files ({}-{} of {})  ↑↓ navigate · Tab/Enter insert · Esc dismiss ",
                scroll_offset + 1,
                visible_end,
                total
            )
        } else {
            format!(" Files ({total})  ↑↓ navigate · Tab/Enter insert · Esc dismiss ")
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::new().fg(self.theme.colors.border.to_color()))
            .title(title);

        if self.matches.is_empty() {
            let text = if self.loading {
                "  Scanning project files..."
            } else {
                "  No matching files"
            };
            let line = Line::from(vec![Span::styled(
                text,
                Style::new().fg(self.theme.colors.text_disabled.to_color()),
            )]);
            Paragraph::new(vec![line]).block(block).render(area, buf);
            return;
        }

        let mut lines: Vec<Line> = Vec::with_capacity(MAX_VISIBLE_ROWS);
        for (offset, path) in self.matches[scroll_offset..visible_end].iter().enumerate() {
            let absolute_index = scroll_offset + offset;
            let is_selected = absolute_index == selected;

            // Directory prefix muted, filename in primary — the eye lands on
            // the basename first (Claude Code's weighting).
            let (dir, name) = match path.rfind('/') {
                // A trailing `/` marks a directory: weight the whole path.
                Some(_) if path.ends_with('/') => ("", path.as_str()),
                Some(idx) => path.split_at(idx + 1),
                None => ("", path.as_str()),
            };
            let dir_style = if is_selected {
                Style::new()
                    .fg(self.theme.colors.text_secondary.to_color())
                    .add_modifier(Modifier::REVERSED)
            } else {
                Style::new().fg(self.theme.colors.text_secondary.to_color())
            };
            let name_style = if is_selected {
                Style::new()
                    .fg(self.theme.colors.text_highlight.to_color())
                    .add_modifier(Modifier::BOLD | Modifier::REVERSED)
            } else {
                Style::new().fg(self.theme.colors.text_primary.to_color())
            };
            lines.push(Line::from(vec![
                Span::styled("  ", dir_style),
                Span::styled(dir.to_string(), dir_style),
                Span::styled(name.to_string(), name_style),
            ]));
        }
        Paragraph::new(lines).block(block).render(area, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render_lines(widget: FilePickerWidget<'_>, width: u16, height: u16) -> Vec<String> {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        widget.render(area, &mut buf);
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn renders_matches_with_count_in_title() {
        let theme = Theme::dark();
        let matches = vec!["src/main.rs".to_string(), "docs/".to_string()];
        let lines = render_lines(
            FilePickerWidget {
                theme: &theme,
                matches: &matches,
                selected_index: 0,
                loading: false,
            },
            60,
            5,
        );
        let all = lines.join("\n");
        assert!(all.contains("Files (2)"), "{all}");
        assert!(all.contains("main.rs"), "{all}");
        assert!(all.contains("docs/"), "{all}");
    }

    #[test]
    fn out_of_range_selection_does_not_panic() {
        let theme = Theme::dark();
        let matches = vec!["a.rs".to_string()];
        let lines = render_lines(
            FilePickerWidget {
                theme: &theme,
                matches: &matches,
                selected_index: 99,
                loading: false,
            },
            40,
            4,
        );
        assert!(lines.join("\n").contains("a.rs"));
    }

    #[test]
    fn empty_states_distinguish_loading_from_no_match() {
        let theme = Theme::dark();
        let loading = render_lines(
            FilePickerWidget {
                theme: &theme,
                matches: &[],
                selected_index: 0,
                loading: true,
            },
            50,
            3,
        )
        .join("\n");
        assert!(loading.contains("Scanning project files"), "{loading}");
        let empty = render_lines(
            FilePickerWidget {
                theme: &theme,
                matches: &[],
                selected_index: 0,
                loading: false,
            },
            50,
            3,
        )
        .join("\n");
        assert!(empty.contains("No matching files"), "{empty}");
    }
}
