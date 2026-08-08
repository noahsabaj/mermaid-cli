//! `/model` picker — renders the bottom zone when `UiMode::ModelPicker` is
//! active.
//!
//! Shaped like the `/load` and `/plan config` panes (bordered, arrow-selectable)
//! with two additions the model list actually needs:
//!
//!   * **Group headings.** Local Ollama models and each remote provider are
//!     visually separated, so "what runs on my machine" is answerable at a
//!     glance — the distinction a sovereignty-focused tool most owes its user.
//!   * **A filter line.** A provider's `/models` endpoint routinely returns
//!     100+ ids. A fixed list of four would be a lie about what is available,
//!     and an unfiltered list of two hundred is unusable; typing narrows it.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};
use unicode_width::UnicodeWidthStr;

use crate::render::theme::Theme;
use mermaid_domain::ModelChoice;

/// Rows drawn at once. Enough to see a provider's block without swallowing the
/// transcript; the window scrolls with the cursor beyond that.
pub const MODEL_PICKER_VISIBLE_ROWS: usize = 10;

/// Total pane height including borders and the filter line.
pub const MODEL_PICKER_HEIGHT: u16 = MODEL_PICKER_VISIBLE_ROWS as u16 + 3;

pub struct ModelPickerWidget<'a> {
    pub theme: &'a Theme,
    /// Rows that survived the filter, in display order.
    pub matches: &'a [&'a ModelChoice],
    /// The live filter text.
    pub query: &'a str,
    pub cursor: usize,
    /// Discovery still running — distinguishes "looking" from "none found".
    pub loading: bool,
    /// The session's active model, marked so the picker always answers "what am
    /// I on right now?" without a second command.
    pub current: &'a str,
}

impl<'a> Widget for ModelPickerWidget<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let c = &self.theme.colors;
        let dim = Style::default().fg(c.text_disabled.to_color());
        let block = Block::default()
            .borders(Borders::ALL)
            .title("Select model — ↑↓ navigate · Enter switch · type to filter · Esc cancel")
            .border_style(Style::default().fg(c.border.to_color()));

        let inner_height = area.height.saturating_sub(2) as usize;
        // One line goes to the filter/status row at the bottom.
        let visible = inner_height
            .saturating_sub(1)
            .min(MODEL_PICKER_VISIBLE_ROWS);

        let mut lines: Vec<Line<'static>> = Vec::new();
        if self.matches.is_empty() {
            lines.push(Line::from(Span::styled(
                if self.loading {
                    "  searching for available models…".to_string()
                } else if self.query.is_empty() {
                    "  No models found. Pull one with `ollama pull`, or set a provider API key."
                        .to_string()
                } else {
                    format!("  Nothing matches {:?}.", self.query)
                },
                dim,
            )));
        } else {
            // Scroll the window to keep the cursor in view.
            let start = self.cursor.saturating_sub(visible.saturating_sub(1));
            let width = area.width.saturating_sub(2) as usize;
            let mut last_group: Option<&str> = None;
            for (i, choice) in self.matches.iter().enumerate().skip(start).take(visible) {
                // A heading only when the group changes AND it is not the very
                // first visible row of a scrolled window (where it would eat a
                // row to restate context the user just scrolled past).
                if last_group != Some(choice.group.as_str()) {
                    last_group = Some(choice.group.as_str());
                    if i > start || start == 0 {
                        lines.push(Line::from(Span::styled(
                            format!(" {}", choice.group),
                            Style::default()
                                .fg(c.header.to_color())
                                .add_modifier(Modifier::BOLD),
                        )));
                    }
                }
                lines.push(row(
                    choice,
                    i == self.cursor,
                    self.current,
                    width,
                    self.theme,
                ));
            }
            lines.truncate(visible);
        }

        // Filter / status footer.
        let footer = if self.query.is_empty() {
            let shown = self.matches.len();
            if self.loading {
                " filter: (type to narrow) · still searching…".to_string()
            } else {
                format!(" filter: (type to narrow) · {shown} models")
            }
        } else {
            format!(
                " filter: {} · {} match{}",
                self.query,
                self.matches.len(),
                if self.matches.len() == 1 { "" } else { "es" }
            )
        };
        lines.push(Line::from(Span::styled(footer, dim)));

        Paragraph::new(lines).block(block).render(area, buf);
    }
}

/// One model row: cursor, id, a `(current)` tag when it is the active model,
/// and the dim detail column right-padded to the pane width.
fn row(
    choice: &ModelChoice,
    highlighted: bool,
    current: &str,
    width: usize,
    theme: &Theme,
) -> Line<'static> {
    let c = &theme.colors;
    let prefix = if highlighted { " > " } else { "   " };
    let id_style = if highlighted {
        Style::default()
            .fg(c.brand.to_color())
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(c.text_primary.to_color())
    };
    // Suffixes are fixed-cost and must survive; the id yields to them. An
    // openrouter id can be 60+ cells on its own, so truncating it is the only
    // way the row fits — and the marker that says "this is your current model"
    // is worth more than the tail of a name.
    // Spelled out rather than a check glyph: Mermaid's output is deliberately
    // emoji-free (enforced by `.github/scripts/check_no_emoji.py`, which flags
    // the whole dingbats block), and a word survives truncation legibly anyway.
    let current_mark = if choice.id == current {
        " (current)"
    } else {
        ""
    };
    let pull_mark = if choice.ready { "" } else { " (not pulled)" };
    let reserved = prefix.width() + current_mark.width() + pull_mark.width();
    let id = super::truncate_to_cells(&choice.id, width.saturating_sub(reserved));

    let mut spans = vec![
        Span::styled(prefix, Style::default().fg(c.brand.to_color())),
        Span::styled(id, id_style),
    ];
    if !current_mark.is_empty() {
        spans.push(Span::styled(
            current_mark,
            Style::default().fg(c.success.to_color()),
        ));
    }
    if !pull_mark.is_empty() {
        spans.push(Span::styled(
            pull_mark,
            Style::default().fg(c.warning.to_color()),
        ));
    }
    // The detail column is a nicety: right-align it only when the row has room
    // left over, and drop it entirely otherwise.
    if !choice.detail.is_empty() {
        let used: usize = spans.iter().map(|s| s.content.width()).sum();
        let detail_width = choice.detail.width();
        if used + detail_width + 2 <= width {
            spans.push(Span::raw(" ".repeat(width - used - detail_width - 1)));
            spans.push(Span::styled(
                choice.detail.clone(),
                Style::default().fg(c.text_disabled.to_color()),
            ));
        }
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choice(id: &str, group: &str) -> ModelChoice {
        ModelChoice {
            id: id.to_string(),
            group: group.to_string(),
            detail: String::new(),
            ready: true,
        }
    }

    fn render_to_string(widget: ModelPickerWidget<'_>, width: u16, height: u16) -> String {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        widget.render(area, &mut buf);
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn marks_the_active_model_and_groups_by_provider() {
        let theme = Theme::dark();
        let local = choice("ollama/llama3.2", "Local (Ollama)");
        let remote = choice("anthropic/claude-opus-4-5", "anthropic");
        let matches = [&local, &remote];
        let out = render_to_string(
            ModelPickerWidget {
                theme: &theme,
                matches: &matches,
                query: "",
                cursor: 0,
                loading: false,
                current: "anthropic/claude-opus-4-5",
            },
            90,
            MODEL_PICKER_HEIGHT,
        );
        assert!(
            out.contains("Local (Ollama)"),
            "group heading missing:\n{out}"
        );
        assert!(
            out.contains("anthropic"),
            "provider heading missing:\n{out}"
        );
        assert!(
            out.contains("claude-opus-4-5 (current)"),
            "the active model must be marked:\n{out}"
        );
        assert!(out.contains("2 models"), "count missing:\n{out}");
    }

    /// A still-running discovery must not read as "there are no models".
    #[test]
    fn loading_and_empty_are_different_messages() {
        let theme = Theme::dark();
        let loading = render_to_string(
            ModelPickerWidget {
                theme: &theme,
                matches: &[],
                query: "",
                cursor: 0,
                loading: true,
                current: "",
            },
            90,
            MODEL_PICKER_HEIGHT,
        );
        assert!(loading.contains("searching"), "{loading}");

        let empty = render_to_string(
            ModelPickerWidget {
                theme: &theme,
                matches: &[],
                query: "",
                cursor: 0,
                loading: false,
                current: "",
            },
            90,
            MODEL_PICKER_HEIGHT,
        );
        assert!(empty.contains("No models found"), "{empty}");
        assert!(!empty.contains("searching"), "{empty}");
    }

    /// Every drawn line must fit the pane — a model id is long and the detail
    /// column is right-aligned against the border.
    #[test]
    fn rows_never_exceed_the_pane_width() {
        let theme = Theme::dark();
        let long = ModelChoice {
            id: "openrouter/some-vendor/a-very-long-model-identifier-that-runs-on".to_string(),
            group: "openrouter".to_string(),
            detail: "context 200k".to_string(),
            ready: true,
        };
        for width in [30usize, 60, 200] {
            let line = row(&long, true, &long.id, width, &theme);
            let drawn: usize = line.spans.iter().map(|s| s.content.width()).sum();
            assert!(
                drawn <= width,
                "row is {drawn} cells wide, pane is {width}: {:?}",
                line.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            );
            // The "you are here" marker survives truncation — it is the one
            // thing the row must never lose.
            assert!(
                line.spans.iter().any(|s| s.content.contains("(current)")),
                "the current-model mark was truncated away at width {width}"
            );
        }
    }
}
