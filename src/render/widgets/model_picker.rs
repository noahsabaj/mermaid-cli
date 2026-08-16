//! `/model` picker — renders the bottom zone when `UiMode::ModelPicker` is
//! active.
//!
//! Shaped like the `/load` and `/plan config` panes (bordered, arrow-selectable)
//! with two additions the model list actually needs:
//!
//!   * **Group headings.** Local Ollama models and each remote provider are
//!     visually separated, so "what runs on my machine" is answerable at a
//!     glance — the distinction a sovereignty-focused tool most owes its user.
//!     The heading is sticky: a window scrolled into the middle of a hundred-row
//!     provider block still names that provider on its first line.
//!   * **Rows without the provider prefix.** The heading already says `nvidia`,
//!     so the row says `mistralai/mistral-large-2-instruct` — and NVIDIA's own
//!     models stop reading `nvidia/nvidia/…`. Nothing is lost: the footer
//!     spells the highlighted row out as the full id `/model` takes.
//!   * **A filter line.** A provider's `/models` endpoint routinely returns
//!     100+ ids. A fixed list of four would be a lie about what is available,
//!     and an unfiltered list of two hundred is unusable; typing narrows it.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};
use unicode_width::UnicodeWidthStr;

use crate::render::theme::{ColorValueExt, Theme};
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
        let width = area.width.saturating_sub(2) as usize;
        // A cursor past the end would only come from a stale frame; clamp
        // rather than panic on the index.
        let cursor = self.cursor.min(self.matches.len().saturating_sub(1));

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
            let start = window_start(self.matches, cursor, visible);
            let mut last_group: Option<&str> = None;
            for (i, choice) in self.matches.iter().enumerate().skip(start) {
                if lines.len() >= visible {
                    break;
                }
                if last_group != Some(choice.group.as_str()) {
                    last_group = Some(choice.group.as_str());
                    // A heading on every group change — and on the first
                    // visible row even mid-group, because the rows no longer
                    // carry the provider themselves.
                    if lines.len() + 2 > visible {
                        // No room for the heading AND its row. Stop rather
                        // than draw the row under the heading above it: an
                        // nvidia model tucked under `meta` is a worse lie
                        // than a blank last line. The one exception is a pane
                        // so short nothing has been drawn yet, where the
                        // cursor's own row still has to appear.
                        if !lines.is_empty() {
                            break;
                        }
                    } else {
                        lines.push(Line::from(Span::styled(
                            format!(" {}", choice.group),
                            Style::default()
                                .fg(c.header.to_color())
                                .add_modifier(Modifier::BOLD),
                        )));
                    }
                }
                lines.push(row(choice, i == cursor, self.current, width, self.theme));
            }
        }

        // Filter / status footer.
        let status = if self.query.is_empty() {
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
        // The highlighted row's id, spelled out in full — the exact string
        // `/model` and `--model` take, which the rows themselves no longer
        // show. When the pane is too narrow for both, the id wins: the count
        // is a nicety, the id is the thing this line exists for.
        let footer = match self.matches.get(cursor) {
            Some(choice) => {
                let both = format!("{status} · {}", choice.id);
                if both.width() <= width {
                    both
                } else {
                    super::truncate_to_cells(&format!(" {}", choice.id), width)
                }
            },
            None => status,
        };
        lines.push(Line::from(Span::styled(footer, dim)));

        Paragraph::new(lines).block(block).render(area, buf);
    }
}

/// First row of the scroll window.
///
/// Not `cursor + 1 - visible`: headings share the pane with rows, so a group
/// boundary inside the window costs a line, and row-only arithmetic pushed the
/// highlighted row past `visible` — where the truncate silently ate it and the
/// picker showed no cursor at all. Walk up from the cursor instead, paying for
/// every row and every heading, and stop when the budget runs out.
fn window_start(matches: &[&ModelChoice], cursor: usize, visible: usize) -> usize {
    let mut start = cursor;
    // The cursor's own row, plus the heading that always sits above it.
    let mut cost = 2usize;
    while start > 0 {
        // Extending upward always adds a row. It adds a heading too only when
        // the row above belongs to another group; within one group the heading
        // already paid for simply moves up.
        let extra = if matches[start - 1].group == matches[start].group {
            1
        } else {
            2
        };
        if cost + extra > visible {
            break;
        }
        cost += extra;
        start -= 1;
    }
    start
}

/// Row text: the id minus the provider segment its heading already states, so
/// `nvidia/mistralai/mistral-large-2-instruct` under the `nvidia` heading reads
/// `mistralai/mistral-large-2-instruct` and NVIDIA's own models stop stuttering
/// `nvidia/nvidia/…`.
///
/// The vendor namespace stays. It is part of the id NIM, OpenRouter, Together
/// and DeepInfra actually take, and it is what tells `mistralai/…` apart from
/// `moonshotai/…`. Only a prefix the heading names is dropped — anything else
/// renders whole rather than hiding a mismatch the user has no way to see.
fn display_id(choice: &ModelChoice) -> &str {
    let Some((prefix, rest)) = choice.id.split_once('/') else {
        return &choice.id;
    };
    if rest.is_empty() {
        return &choice.id;
    }
    let group = choice.group.to_ascii_lowercase();
    let prefix = prefix.to_ascii_lowercase();
    // A remote group IS the provider name; the local group spells it out as
    // `Local (Ollama)`.
    if group == prefix || group.contains(&format!("({prefix})")) {
        rest
    } else {
        &choice.id
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
    let id = super::truncate_to_cells(display_id(choice), width.saturating_sub(reserved));

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

    /// The frame minus its filter/status line — what the *rows* say, which is
    /// the only place the provider prefix is supposed to be gone.
    fn without_footer(frame: &str) -> String {
        frame
            .lines()
            .filter(|l| !l.contains("filter:"))
            .collect::<Vec<_>>()
            .join("\n")
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

    /// The heading names the provider, so the row must not repeat it — the
    /// shape that made NVIDIA's own models render `nvidia/nvidia/…`.
    #[test]
    fn rows_drop_the_provider_the_heading_already_names() {
        let theme = Theme::dark();
        let local = choice("ollama/gemma4:e4b-it-qat", "Local (Ollama)");
        let own = choice("nvidia/nvidia/nemotron-3-super-120b-a12b", "nvidia");
        let vendor = choice("nvidia/mistralai/mistral-large-2-instruct", "nvidia");
        let matches = [&local, &own, &vendor];
        let out = render_to_string(
            ModelPickerWidget {
                theme: &theme,
                matches: &matches,
                query: "",
                cursor: 0,
                loading: false,
                current: "",
            },
            90,
            MODEL_PICKER_HEIGHT,
        );
        // Rows only: the footer carries the highlighted id in full by design.
        let rows = without_footer(&out);
        assert!(
            !rows.contains("nvidia/nvidia/"),
            "the stutter is back:\n{out}"
        );
        assert!(
            !rows.contains("ollama/gemma4"),
            "local rows repeat too:\n{out}"
        );
        assert!(out.contains("gemma4:e4b-it-qat"), "{out}");
        // The vendor namespace is part of the upstream id and stays put.
        assert!(out.contains("mistralai/mistral-large-2-instruct"), "{out}");
        assert!(out.contains("nvidia/nemotron-3-super-120b-a12b"), "{out}");
    }

    /// Eliding is only safe when the heading really does name the prefix;
    /// otherwise the row would hide a provider the user cannot see anywhere.
    #[test]
    fn a_prefix_the_heading_does_not_name_is_kept() {
        let mismatched = choice("openrouter/z-ai/glm-5.2", "nvidia");
        assert_eq!(display_id(&mismatched), "openrouter/z-ai/glm-5.2");
        let bare = choice("llama3.2", "Local (Ollama)");
        assert_eq!(display_id(&bare), "llama3.2");
        let local = choice("ollama/llama3.2", "Local (Ollama)");
        assert_eq!(display_id(&local), "llama3.2");
    }

    /// The footer is where the full `provider/vendor/model` string lives now,
    /// so what to type into `--model` is never more than a glance away.
    #[test]
    fn the_footer_spells_out_the_highlighted_id_in_full() {
        let theme = Theme::dark();
        let own = choice("nvidia/nvidia/nemotron-3-super-120b-a12b", "nvidia");
        let matches = [&own];
        for width in [90u16, 44] {
            let out = render_to_string(
                ModelPickerWidget {
                    theme: &theme,
                    matches: &matches,
                    query: "",
                    cursor: 0,
                    loading: false,
                    current: "",
                },
                width,
                MODEL_PICKER_HEIGHT,
            );
            // Rows elide the prefix, so a full id on screen can only be the
            // footer's. The narrow pane drops the count to keep it.
            assert!(
                out.contains("nvidia/nvidia/nemotron"),
                "the full id is not on screen at width {width}:\n{out}"
            );
        }
    }

    /// A window scrolled into the middle of a hundred-row provider block must
    /// still name the provider: the rows no longer carry it themselves.
    #[test]
    fn a_scrolled_window_still_names_its_provider() {
        let theme = Theme::dark();
        let owned: Vec<ModelChoice> = (0..40)
            .map(|i| choice(&format!("nvidia/mistralai/model-{i:02}"), "nvidia"))
            .collect();
        let matches: Vec<&ModelChoice> = owned.iter().collect();
        let out = render_to_string(
            ModelPickerWidget {
                theme: &theme,
                matches: &matches,
                query: "",
                cursor: 30,
                loading: false,
                current: "",
            },
            90,
            MODEL_PICKER_HEIGHT,
        );
        assert!(
            out.lines()
                .any(|l| l.trim_matches(|ch| ch == '│' || ch == ' ') == "nvidia"),
            "no heading line names the provider:\n{out}"
        );
        assert!(out.contains("> mistralai/model-30"), "{out}");
    }

    /// A row may never sit under another group's heading. When the boundary
    /// falls on the last visible line there is no room for the new heading,
    /// and drawing the row anyway filed NVIDIA's catalog under `meta` — which
    /// a stripped row has no way to contradict.
    #[test]
    fn no_row_is_filed_under_another_groups_heading() {
        let theme = Theme::dark();
        let mut owned: Vec<ModelChoice> = (0..8)
            .map(|i| choice(&format!("ollama/local-{i}"), "Local (Ollama)"))
            .collect();
        owned.push(choice("nvidia/mistralai/remote-0", "nvidia"));
        let matches: Vec<&ModelChoice> = owned.iter().collect();
        let out = render_to_string(
            ModelPickerWidget {
                theme: &theme,
                matches: &matches,
                query: "",
                cursor: 0,
                loading: false,
                current: "",
            },
            90,
            MODEL_PICKER_HEIGHT,
        );
        let rows = without_footer(&out);
        assert!(
            rows.contains("local-7"),
            "the local block is cut short:\n{out}"
        );
        // The row's own text no longer names its provider, so the heading is
        // the only thing that can: either both are on screen, or neither is.
        assert!(
            !rows.contains("remote-0") || rows.contains("nvidia"),
            "an nvidia row is on screen with no nvidia heading:\n{out}"
        );
    }

    /// A group boundary inside the window costs a line. Sizing the window in
    /// rows alone pushed the highlighted row past the last one, and the
    /// truncate ate it — the picker rendered with no visible cursor.
    #[test]
    fn the_highlighted_row_survives_a_group_boundary() {
        let theme = Theme::dark();
        let mut owned: Vec<ModelChoice> = (0..8)
            .map(|i| choice(&format!("ollama/local-{i}"), "Local (Ollama)"))
            .collect();
        owned.extend((0..8).map(|i| choice(&format!("nvidia/mistralai/remote-{i}"), "nvidia")));
        let matches: Vec<&ModelChoice> = owned.iter().collect();
        for cursor in 0..matches.len() {
            let out = render_to_string(
                ModelPickerWidget {
                    theme: &theme,
                    matches: &matches,
                    query: "",
                    cursor,
                    loading: false,
                    current: "",
                },
                90,
                MODEL_PICKER_HEIGHT,
            );
            // The marker, not the bare id: the footer also carries the id.
            let marked = format!("> {}", display_id(matches[cursor]));
            assert!(
                out.contains(&marked),
                "row {cursor} is not on screen as {marked:?}:\n{out}"
            );
        }
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
