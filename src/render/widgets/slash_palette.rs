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

use crate::render::theme::Theme;
use mermaid_domain::slash_commands::PaletteEntry;

/// Hard cap on visible rows — anything beyond is hidden until the user
/// narrows the filter. Current registry has 9 entries; cap at 8 means
/// at most one row is hidden when filter is empty. If the registry
/// grows past ~12 we should add scrolling.
const MAX_VISIBLE_ROWS: usize = 8;

pub struct SlashPaletteWidget<'a> {
    pub theme: &'a Theme,
    /// Already-filtered (and ordered) list of rows to display — built-ins
    /// plus plugin prompt commands, from `filter_entries` so indices agree
    /// with the reducer's cursor.
    pub entries: Vec<PaletteEntry<'a>>,
    /// Index into `commands` of the highlighted row. `render` clamps it to
    /// the valid range (or 0 when empty), so an out-of-range value from the
    /// caller can't panic the row slice (#103).
    pub selected_index: usize,
}

impl<'a> Widget for SlashPaletteWidget<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // Scroll window: when selected row falls outside the visible
        // 8-row band, slide the window so selected stays in view.
        // "Anchor at bottom" — once selected goes past row 7, the
        // selection sits at the bottom row of the visible window. Same
        // pattern as most terminal palettes (fzf, less +F).
        let total = self.entries.len();
        // Clamp defensively: an out-of-range `selected_index` would drive
        // `scroll_offset` past `visible_end` and panic the
        // `commands[scroll_offset..visible_end]` slice below (#103).
        let selected = self.selected_index.min(total.saturating_sub(1));
        let scroll_offset = if selected >= MAX_VISIBLE_ROWS {
            selected + 1 - MAX_VISIBLE_ROWS
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
            format!(" Commands ({total})  ↑↓ navigate · Tab complete · Esc dismiss ")
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::new().fg(self.theme.colors.border.to_color()))
            .title(title);

        // Empty filter result: render one line of explanatory text so
        // the user understands their typed prefix matched nothing.
        if self.entries.is_empty() {
            let line = Line::from(vec![Span::styled(
                "  No matching commands",
                Style::new().fg(self.theme.colors.text_disabled.to_color()),
            )]);
            Paragraph::new(vec![line]).block(block).render(area, buf);
            return;
        }

        let mut lines: Vec<Line> = Vec::with_capacity(MAX_VISIBLE_ROWS);
        for (offset, entry) in self.entries[scroll_offset..visible_end].iter().enumerate() {
            // Recover the absolute index for selection comparison.
            let absolute_index = scroll_offset + offset;
            let is_selected = absolute_index == selected;

            // Build the `/name [arg_hint]` chunk. The arg_hint is in a
            // softer color so the eye lands on the command name first.
            let mut name_part = format!("/{}", entry.name());
            if let Some(hint) = entry.arg_hint() {
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
            let padded_name = format!(" {name_part:<22}");
            lines.push(Line::from(vec![
                Span::styled(padded_name, name_style),
                Span::styled(format!(" {}", entry.description()), desc_style),
            ]));
        }

        Paragraph::new(lines).block(block).render(area, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;

    #[test]
    fn out_of_bounds_selection_does_not_panic() {
        // #103: a caller that lets `selected_index` exceed the filtered list
        // must not panic the `commands[scroll_offset..visible_end]` slice.
        let theme = Theme::dark();
        let entries = mermaid_domain::slash_commands::filter_entries("", &[]);
        assert!(!entries.is_empty(), "registry should expose commands");
        let widget = SlashPaletteWidget {
            theme: &theme,
            entries,
            selected_index: 9999,
        };
        let backend = TestBackend::new(80, 12);
        let mut term = Terminal::new(backend).expect("terminal");
        term.draw(|f| f.render_widget(widget, Rect::new(0, 0, 80, 12)))
            .expect("render must not panic on OOB selection");
    }
}
