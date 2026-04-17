//! Slash-command palette widget — renders a filter-as-you-type list of
//! available commands with the selected row highlighted. Visible
//! whenever the input starts with `/`; replaces the bottom status bar
//! while open (same screen region — see `render.rs::render_ui`).
//!
//! Keyboard handling lives in `event_handler.rs::handle_palette_key`.
//! This widget is purely presentational — it consumes a pre-filtered
//! slice and a selection index.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Widget},
};

use crate::tui::slash_commands::SlashCommand;
use crate::tui::theme::Theme;

/// Hard cap on visible rows — anything beyond is hidden until the user
/// narrows the filter. Current registry has 9 entries; cap at 8 means
/// at most one row is hidden when filter is empty. If the registry
/// grows past ~12 we should add scrolling.
const MAX_VISIBLE_ROWS: usize = 8;

pub struct SlashPaletteWidget<'a> {
    pub theme: &'a Theme,
    /// Already-filtered (and ordered) list of commands to display.
    pub commands: Vec<&'static SlashCommand>,
    /// Index into `commands` of the highlighted row. Caller is
    /// responsible for keeping it in bounds (or setting 0 on empty).
    pub selected_index: usize,
}

impl<'a> Widget for SlashPaletteWidget<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // Scroll window: when selected row falls outside the visible
        // 8-row band, slide the window so selected stays in view.
        // "Anchor at bottom" — once selected goes past row 7, the
        // selection sits at the bottom row of the visible window. Same
        // pattern as most terminal palettes (fzf, less +F).
        let total = self.commands.len();
        let scroll_offset = if self.selected_index >= MAX_VISIBLE_ROWS {
            self.selected_index + 1 - MAX_VISIBLE_ROWS
        } else {
            0
        };
        let visible_end = (scroll_offset + MAX_VISIBLE_ROWS).min(total);

        // Title: show total count + indicator when scrolled, so users
        // know there's content above/below the visible window.
        let title = if total > MAX_VISIBLE_ROWS {
            format!(
                " Commands ({}-{} of {})  ↑↓ navigate · Tab complete · Esc dismiss ",
                scroll_offset + 1,
                visible_end,
                total
            )
        } else {
            format!(
                " Commands ({})  ↑↓ navigate · Tab complete · Esc dismiss ",
                total
            )
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::new().fg(self.theme.colors.border.to_color()))
            .title(title);

        // Empty filter result: render one line of explanatory text so
        // the user understands their typed prefix matched nothing.
        if self.commands.is_empty() {
            let line = Line::from(vec![Span::styled(
                "  No matching commands",
                Style::new().fg(self.theme.colors.text_disabled.to_color()),
            )]);
            Paragraph::new(vec![line]).block(block).render(area, buf);
            return;
        }

        let mut lines: Vec<Line> = Vec::with_capacity(MAX_VISIBLE_ROWS);
        for (offset, cmd) in self.commands[scroll_offset..visible_end].iter().enumerate() {
            // Recover the absolute index for selection comparison.
            let absolute_index = scroll_offset + offset;
            let is_selected = absolute_index == self.selected_index;

            // Build the `/name [arg_hint]` chunk. The arg_hint is in a
            // softer color so the eye lands on the command name first.
            let mut name_part = format!("/{}", cmd.name);
            if let Some(hint) = cmd.arg_hint {
                name_part.push(' ');
                name_part.push_str(hint);
            }

            let name_style = if is_selected {
                Style::new()
                    .fg(self.theme.colors.text_highlight.to_color())
                    .add_modifier(Modifier::BOLD | Modifier::REVERSED)
            } else {
                Style::new()
                    .fg(self.theme.colors.info.to_color())
                    .add_modifier(Modifier::BOLD)
            };
            let desc_style = if is_selected {
                Style::new()
                    .fg(self.theme.colors.text_primary.to_color())
                    .add_modifier(Modifier::REVERSED)
            } else {
                Style::new().fg(self.theme.colors.text_secondary.to_color())
            };

            // Pad command column so descriptions align.
            let padded_name = format!(" {:<22}", name_part);
            lines.push(Line::from(vec![
                Span::styled(padded_name, name_style),
                Span::styled(format!(" {}", cmd.description), desc_style),
            ]));
        }

        Paragraph::new(lines).block(block).render(area, buf);
    }
}
