//! Bottom-zone modal for the `ask_user_question` tool.
//!
//! Renders a batch of questions Claude-Code-style: a header chip, the question
//! text, numbered options with a `>` cursor (and `[x]`/`[ ]` for multi-select),
//! an "Other" free-text row, and muted footer hints. Batched questions get a
//! top tab strip and a final "Review your answers" screen. Presentational only
//! — all selection state lives on `PendingQuestionSet` and is driven by the
//! reducer's `handle_question_key`.

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};

use super::truncate_to_cells;
use crate::domain::{OptionPreview, PendingQuestionSet};
use crate::render::theme::Theme;

pub struct QuestionModalWidget<'a> {
    pub theme: &'a Theme,
    pub set: &'a PendingQuestionSet,
}

/// A header "chip": label on the brand accent, like Claude Code's blue tag.
fn chip(label: &str, bg: Color) -> Span<'static> {
    Span::styled(
        format!(" {} ", label),
        Style::default()
            .fg(Color::Black)
            .bg(bg)
            .add_modifier(Modifier::BOLD),
    )
}

/// Build the modal's content lines. Shared by the widget and the height
/// estimator so the reserved bottom-zone height always matches what's drawn.
pub fn build_question_lines(set: &PendingQuestionSet, theme: &Theme) -> Vec<Line<'static>> {
    let brand = theme.colors.brand.to_color();
    let dim = theme.colors.text_disabled.to_color();
    let white = Color::White;
    let nq = set.questions.len();
    let mut lines: Vec<Line<'static>> = Vec::new();

    // Tab strip for batched questions: chips for each question plus a trailing
    // Submit; the active one is highlighted.
    if nq > 1 {
        let mut spans: Vec<Span<'static>> = vec![Span::styled("< ", Style::default().fg(dim))];
        for (qi, q) in set.questions.iter().enumerate() {
            let label = truncate_to_cells(&q.header, 12);
            if qi == set.active {
                spans.push(chip(&label, brand));
            } else {
                spans.push(Span::styled(format!(" {} ", label), Style::default().fg(dim)));
            }
            spans.push(Span::raw(" "));
        }
        if set.active >= nq {
            spans.push(chip("Submit", brand));
        } else {
            spans.push(Span::styled(" Submit ", Style::default().fg(dim)));
        }
        spans.push(Span::styled(" >", Style::default().fg(dim)));
        lines.push(Line::from(spans));
        lines.push(Line::from(""));
    }

    // Review screen.
    if set.active >= nq {
        lines.push(Line::from(Span::styled(
            "Review your answers",
            Style::default().fg(white).add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(""));
        for (q, ans) in set.questions.iter().zip(set.build_answers()) {
            lines.push(Line::from(Span::styled(
                format!("- {}", q.question),
                Style::default().fg(white),
            )));
            let value = if ans.selected.is_empty() {
                "(no selection)".to_string()
            } else {
                ans.selected.join(", ")
            };
            lines.push(Line::from(Span::styled(
                format!("   -> {}", value),
                Style::default().fg(brand),
            )));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Ready to submit your answers?",
            Style::default().fg(dim),
        )));
        for (i, opt) in ["1. Submit answers", "2. Cancel"].iter().enumerate() {
            let focused = set.review_cursor == i;
            let style = if focused {
                Style::default().fg(brand).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(white)
            };
            lines.push(Line::from(vec![
                Span::styled(
                    if focused { "> " } else { "  " },
                    Style::default().fg(brand),
                ),
                Span::styled((*opt).to_string(), style),
            ]));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Enter to select | Up/Down to navigate | Esc to cancel",
            Style::default().fg(dim),
        )));
        return lines;
    }

    // A single question tab.
    let q = &set.questions[set.active];
    let sel = &set.selections[set.active];
    let n = q.options.len();
    let multi = q.multi_select;

    lines.push(Line::from(chip(&truncate_to_cells(&q.header, 12), brand)));
    lines.push(Line::from(Span::styled(
        q.question.clone(),
        Style::default().fg(white).add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));

    // Options.
    for (i, opt) in q.options.iter().enumerate() {
        let focused = sel.cursor == i;
        let checked = sel.chosen.contains(&i);
        let mut spans: Vec<Span<'static>> = vec![
            Span::styled(if focused { "> " } else { "  " }, Style::default().fg(brand)),
            Span::styled(format!("{}. ", i + 1), Style::default().fg(dim)),
        ];
        if multi {
            spans.push(Span::styled(
                if checked { "[x] " } else { "[ ] " },
                Style::default().fg(if checked { brand } else { dim }),
            ));
        }
        let label_style = if focused {
            Style::default().fg(brand).add_modifier(Modifier::BOLD)
        } else if !multi && checked {
            Style::default().fg(brand)
        } else {
            Style::default().fg(white).add_modifier(Modifier::BOLD)
        };
        spans.push(Span::styled(opt.label.clone(), label_style));
        lines.push(Line::from(spans));
        if let Some(desc) = &opt.description {
            lines.push(Line::from(Span::styled(
                format!("     {}", desc),
                Style::default().fg(dim),
            )));
        }
    }

    // "Other" free-text row — the universal escape hatch.
    {
        let focused = sel.cursor == n;
        let mut spans: Vec<Span<'static>> = vec![
            Span::styled(if focused { "> " } else { "  " }, Style::default().fg(brand)),
            Span::styled(format!("{}. ", n + 1), Style::default().fg(dim)),
        ];
        if multi {
            let checked = !sel.other_text.trim().is_empty();
            spans.push(Span::styled(
                if checked { "[x] " } else { "[ ] " },
                Style::default().fg(if checked { brand } else { dim }),
            ));
        }
        if sel.other_text.is_empty() {
            spans.push(Span::styled("Type something", Style::default().fg(dim)));
        } else {
            spans.push(Span::styled(sel.other_text.clone(), Style::default().fg(white)));
        }
        lines.push(Line::from(spans));
    }

    // Submit row (multi-select only).
    if multi {
        let focused = sel.cursor == n + 1;
        let style = if focused {
            Style::default().fg(brand).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(white).add_modifier(Modifier::BOLD)
        };
        lines.push(Line::from(vec![
            Span::styled(if focused { "> " } else { "  " }, Style::default().fg(brand)),
            Span::styled("Submit", style),
        ]));
    }

    // Notes line (press `n` to edit).
    lines.push(Line::from(""));
    if set.editing_note {
        lines.push(Line::from(vec![
            Span::styled("Notes: ", Style::default().fg(brand)),
            Span::styled(sel.note.clone(), Style::default().fg(white)),
            Span::styled("_", Style::default().fg(brand)),
        ]));
    } else if !sel.note.trim().is_empty() {
        lines.push(Line::from(vec![
            Span::styled("Notes: ", Style::default().fg(dim)),
            Span::styled(sel.note.clone(), Style::default().fg(white)),
        ]));
    } else {
        lines.push(Line::from(Span::styled(
            "Notes: press n to add notes",
            Style::default().fg(dim),
        )));
    }

    // Footer hints.
    lines.push(Line::from(""));
    let mut hint = String::from("Enter to select | Up/Down to navigate");
    if nq > 1 {
        hint.push_str(" | Tab to switch");
    }
    hint.push_str(" | n: notes | Esc to cancel");
    lines.push(Line::from(Span::styled(hint, Style::default().fg(dim))));

    lines
}

/// The preview to show in the right pane: the focused option's preview on the
/// active question tab (none on the review screen or on the Other/Submit rows).
fn focused_preview(set: &PendingQuestionSet) -> Option<&OptionPreview> {
    if set.active >= set.questions.len() {
        return None;
    }
    let q = &set.questions[set.active];
    let cursor = set.selections[set.active].cursor;
    q.options.get(cursor).and_then(|o| o.preview.as_ref())
}

/// Whether the active question has any option with a preview — drives the
/// side-by-side layout (kept stable as the cursor moves between options).
fn active_question_has_preview(set: &PendingQuestionSet) -> bool {
    set.active < set.questions.len()
        && set.questions[set.active]
            .options
            .iter()
            .any(|o| o.preview.is_some())
}

/// Tallest preview among the active question's options, so the modal height is
/// stable while arrowing through options.
fn max_preview_lines(set: &PendingQuestionSet) -> usize {
    if set.active >= set.questions.len() {
        return 0;
    }
    set.questions[set.active]
        .options
        .iter()
        .filter_map(|o| o.preview.as_ref())
        .map(|p| p.content.lines().count())
        .max()
        .unwrap_or(0)
}

/// Render a preview body: plain monospace, or a unified diff with `+` green,
/// `-` red, and `@@` hunk headers cyan.
fn build_preview_lines(preview: &OptionPreview, theme: &Theme) -> Vec<Line<'static>> {
    let add = theme.colors.success.to_color();
    let rem = theme.colors.error.to_color();
    let hunk = theme.colors.info.to_color();
    let base = theme.colors.code_foreground.to_color();
    preview
        .content
        .lines()
        .map(|raw| {
            let style = if preview.diff {
                match raw.chars().next() {
                    Some('+') => Style::default().fg(add),
                    Some('-') => Style::default().fg(rem),
                    Some('@') => Style::default().fg(hunk),
                    _ => Style::default().fg(base),
                }
            } else {
                Style::default().fg(base)
            };
            Line::from(Span::styled(raw.to_string(), style))
        })
        .collect()
}

/// Total rendered height including the border, so `render::mod` can size the
/// bottom zone to fit the modal — the taller of the option list and its preview.
pub fn question_modal_height(set: &PendingQuestionSet, theme: &Theme) -> u16 {
    let left = build_question_lines(set, theme).len();
    let content = if active_question_has_preview(set) {
        left.max(max_preview_lines(set))
    } else {
        left
    };
    (content as u16).saturating_add(2)
}

impl<'a> Widget for QuestionModalWidget<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let brand = self.theme.colors.brand.to_color();
        let dim = self.theme.colors.text_disabled.to_color();
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(brand));
        let inner = block.inner(area);
        block.render(area, buf);

        if active_question_has_preview(self.set) {
            // Side-by-side: options on the left, focused option's preview right.
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(48), Constraint::Percentage(52)])
                .split(inner);
            Paragraph::new(build_question_lines(self.set, self.theme)).render(cols[0], buf);
            let preview_lines = focused_preview(self.set)
                .map(|p| build_preview_lines(p, self.theme))
                .unwrap_or_default();
            Paragraph::new(preview_lines)
                .block(
                    Block::default()
                        .borders(Borders::LEFT)
                        .border_style(Style::default().fg(dim)),
                )
                .render(cols[1], buf);
        } else {
            Paragraph::new(build_question_lines(self.set, self.theme)).render(inner, buf);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Question, QuestionOption, ToolCallId, TurnId};

    fn opt_with_preview(label: &str, preview: Option<OptionPreview>) -> QuestionOption {
        QuestionOption {
            label: label.to_string(),
            description: None,
            recommended: false,
            preview,
        }
    }

    #[test]
    fn diff_preview_colors_hunk_remove_and_add_lines() {
        let theme = Theme::dark();
        let preview = OptionPreview {
            content: "@@ -1 +1 @@\n-old\n+new\n context".to_string(),
            language: None,
            diff: true,
        };
        let lines = build_preview_lines(&preview, &theme);
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0].spans[0].style.fg, Some(theme.colors.info.to_color()));
        assert_eq!(lines[1].spans[0].style.fg, Some(theme.colors.error.to_color()));
        assert_eq!(lines[2].spans[0].style.fg, Some(theme.colors.success.to_color()));
    }

    #[test]
    fn tall_preview_drives_modal_height() {
        let body = std::iter::repeat_n("line", 20).collect::<Vec<_>>().join("\n");
        let q = Question {
            header: "H".to_string(),
            question: "Q?".to_string(),
            multi_select: false,
            options: vec![
                opt_with_preview(
                    "A",
                    Some(OptionPreview {
                        content: body,
                        language: None,
                        diff: false,
                    }),
                ),
                opt_with_preview("B", None),
            ],
        };
        let set = PendingQuestionSet::new(TurnId(1), ToolCallId(1), vec![q]);
        let theme = Theme::dark();
        // Modal grows to fit the 20-line preview (plus the 2-line border).
        assert!(
            question_modal_height(&set, &theme) >= 22,
            "expected height >= 22 to fit the preview"
        );
    }
}
