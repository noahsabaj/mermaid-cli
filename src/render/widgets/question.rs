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
use unicode_width::UnicodeWidthStr;

use super::truncate_to_cells;
use crate::render::theme::Theme;
use crate::render::widgets::chat::wrap_styled_line;
use mermaid_model::question::{OptionPreview, PendingQuestionSet, Question, QuestionSelection};

pub struct QuestionModalWidget<'a> {
    pub theme: &'a Theme,
    pub set: &'a PendingQuestionSet,
    /// Total width of the modal INCLUDING its border, so the content wraps to
    /// the same width the height estimator assumed.
    pub width: u16,
}

/// Content width available to the question column, derived from the modal's
/// total width. The estimator and the renderer must agree or the reserved
/// bottom zone won't match what's drawn, so both go through here.
///
/// With a preview open the questions get the left 48% split; the estimator's
/// integer share is never wider than the layout solver's column, so any drift
/// makes the estimate wrap MORE and the modal a line too tall rather than a
/// line too short (which would clip).
fn question_column_width(set: &PendingQuestionSet, total_width: u16) -> usize {
    let inner = total_width.saturating_sub(2) as usize;
    if active_question_has_preview(set) {
        inner * 48 / 100
    } else {
        inner
    }
    .max(8)
}

/// Push a line, wrapping it to `width` with `hang` cells of hanging indent so
/// a long option description continues under its own text instead of running
/// off the border. Everything the modal draws goes through this — a modal is
/// a fixed box, so nothing may rely on the terminal to clip it.
fn push_wrapped(lines: &mut Vec<Line<'static>>, line: Line<'static>, width: usize, hang: usize) {
    lines.extend(wrap_styled_line(
        line,
        width,
        hang.min(width.saturating_sub(1)),
    ));
}

/// A header "chip": label on the brand accent, like Claude Code's blue tag.
/// `fg` is the theme background so the label stays readable on the accent.
fn chip(label: &str, fg: Color, bg: Color) -> Span<'static> {
    Span::styled(
        format!(" {} ", label),
        Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD),
    )
}

/// Scroll window for a long option list: `(start, end)` indices to render,
/// keeping the cursor visible. Rows beyond the window get "N more" markers.
fn option_window(cursor: usize, n: usize, max: usize) -> (usize, usize) {
    if n <= max {
        return (0, n);
    }
    let c = cursor.min(n - 1);
    let start = c.saturating_sub(max / 2).min(n - max);
    (start, start + max)
}

fn input_placeholder(kind: &mermaid_domain::QuestionKind) -> &'static str {
    match kind {
        mermaid_domain::QuestionKind::Number { .. } => "a number",
        mermaid_domain::QuestionKind::Date => "YYYY-MM-DD",
        mermaid_domain::QuestionKind::Path { .. } => "a path",
        _ => "type a value",
    }
}

/// Render Select/MultiSelect options (with a scroll window for long lists), the
/// Other row, and the Submit row (multi-select only).
fn push_choice_lines(
    lines: &mut Vec<Line<'static>>,
    q: &Question,
    sel: &QuestionSelection,
    theme: &Theme,
    width: usize,
) {
    let brand = theme.colors.brand.to_color();
    let dim = theme.colors.text_disabled.to_color();
    let white = theme.colors.text_primary.to_color();
    let n = q.options.len();
    let multi = q.is_multi();
    const MAX_VISIBLE: usize = 8;
    let (start, end) = option_window(sel.cursor, n, MAX_VISIBLE);
    if start > 0 {
        lines.push(Line::from(Span::styled(
            format!("  ... {start} more above"),
            Style::default().fg(dim),
        )));
    }
    for i in start..end {
        let opt = &q.options[i];
        let focused = sel.cursor == i;
        let checked = sel.chosen.contains(&i);
        let mut spans: Vec<Span<'static>> = vec![
            Span::styled(
                if focused { "> " } else { "  " },
                Style::default().fg(brand),
            ),
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
        // Everything before the label is gutter: "> " + "N. " (+ "[x] ").
        let hang: usize = spans.iter().map(|s| s.content.width()).sum();
        spans.push(Span::styled(opt.label.clone(), label_style));
        push_wrapped(lines, Line::from(spans), width, hang);
        if let Some(desc) = &opt.description {
            push_wrapped(
                lines,
                Line::from(Span::styled(
                    format!("     {}", desc),
                    Style::default().fg(dim),
                )),
                width,
                5,
            );
        }
    }
    if end < n {
        lines.push(Line::from(Span::styled(
            format!("  ... {} more below", n - end),
            Style::default().fg(dim),
        )));
    }

    // "Other" free-text row.
    let other_focused = sel.cursor == n;
    let mut spans: Vec<Span<'static>> = vec![
        Span::styled(
            if other_focused { "> " } else { "  " },
            Style::default().fg(brand),
        ),
        Span::styled(format!("{}. ", n + 1), Style::default().fg(dim)),
    ];
    if multi {
        let checked = !sel.other_text.trim().is_empty();
        spans.push(Span::styled(
            if checked { "[x] " } else { "[ ] " },
            Style::default().fg(if checked { brand } else { dim }),
        ));
    }
    let hang: usize = spans.iter().map(|s| s.content.width()).sum();
    if sel.other_text.is_empty() {
        spans.push(Span::styled("Type something", Style::default().fg(dim)));
    } else {
        spans.push(Span::styled(
            sel.other_text.clone(),
            Style::default().fg(white),
        ));
    }
    push_wrapped(lines, Line::from(spans), width, hang);

    // Submit row (multi-select only).
    if multi {
        let focused = sel.cursor == n + 1;
        let style = if focused {
            Style::default().fg(brand).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(white).add_modifier(Modifier::BOLD)
        };
        lines.push(Line::from(vec![
            Span::styled(
                if focused { "> " } else { "  " },
                Style::default().fg(brand),
            ),
            Span::styled("Submit", style),
        ]));
    }
}

/// Render an input-kind value field, an optional Number slider bar, and
/// live validation feedback.
fn push_input_lines(
    lines: &mut Vec<Line<'static>>,
    q: &Question,
    sel: &QuestionSelection,
    theme: &Theme,
    max_width: usize,
) {
    let brand = theme.colors.brand.to_color();
    let dim = theme.colors.text_disabled.to_color();
    let white = theme.colors.text_primary.to_color();
    let value = &sel.value;
    let mut field: Vec<Span<'static>> = vec![Span::styled("> ", Style::default().fg(brand))];
    if value.is_empty() {
        field.push(Span::styled(
            input_placeholder(&q.kind),
            Style::default().fg(dim),
        ));
    } else {
        field.push(Span::styled(value.clone(), Style::default().fg(white)));
    }
    field.push(Span::styled("_", Style::default().fg(brand)));
    push_wrapped(lines, Line::from(field), max_width, 2);

    if let mermaid_domain::QuestionKind::Number {
        min: Some(lo),
        max: Some(hi),
        slider: true,
        ..
    } = &q.kind
        && hi > lo
    {
        let cur: f64 = value.trim().parse().unwrap_or(*lo);
        let frac = ((cur - lo) / (hi - lo)).clamp(0.0, 1.0);
        let width = 20usize;
        let filled = (frac * width as f64).round() as usize;
        let bar = format!("[{}{}]", "#".repeat(filled), "-".repeat(width - filled));
        lines.push(Line::from(Span::styled(
            format!("  {bar}"),
            Style::default().fg(brand),
        )));
    }

    match mermaid_domain::validate_input(&q.kind, value) {
        Err(e) => push_wrapped(
            lines,
            Line::from(Span::styled(
                format!("  {e}"),
                Style::default().fg(theme.colors.error.to_color()),
            )),
            max_width,
            2,
        ),
        Ok(()) => lines.push(Line::from(Span::styled(
            "  Enter to submit",
            Style::default().fg(dim),
        ))),
    }
}

/// Render a Rank question's options in their current order, with a grab marker.
fn push_rank_lines(
    lines: &mut Vec<Line<'static>>,
    q: &Question,
    sel: &QuestionSelection,
    theme: &Theme,
    width: usize,
) {
    let brand = theme.colors.brand.to_color();
    let dim = theme.colors.text_disabled.to_color();
    let white = theme.colors.text_primary.to_color();
    for (pos, &opt_idx) in mermaid_domain::rank_order(q, sel).iter().enumerate() {
        let focused = sel.cursor == pos;
        let grabbed = focused && sel.grabbed;
        let prefix = if grabbed {
            ">>"
        } else if focused {
            "> "
        } else {
            "  "
        };
        let label_style = if focused {
            Style::default().fg(brand).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(white).add_modifier(Modifier::BOLD)
        };
        let marker = format!("{}. ", pos + 1);
        let hang = prefix.width() + marker.width();
        push_wrapped(
            lines,
            Line::from(vec![
                Span::styled(prefix, Style::default().fg(brand)),
                Span::styled(marker, Style::default().fg(dim)),
                Span::styled(
                    q.options
                        .get(opt_idx)
                        .map(|o| o.label.clone())
                        .unwrap_or_default(),
                    label_style,
                ),
            ]),
            width,
            hang,
        );
    }
}

/// Build the modal's content lines. Shared by the widget and the height
/// estimator so the reserved bottom-zone height always matches what's drawn.
#[expect(
    clippy::too_many_lines,
    reason = "predates the lint; see .github/baselines/expect_budget.txt"
)]
pub fn build_question_lines(
    set: &PendingQuestionSet,
    theme: &Theme,
    width: usize,
) -> Vec<Line<'static>> {
    let brand = theme.colors.brand.to_color();
    let dim = theme.colors.text_disabled.to_color();
    let white = theme.colors.text_primary.to_color();
    let nq = set.questions.len();
    let has_memory = set.questions.iter().any(|q| q.memory_key.is_some());
    let mut lines: Vec<Line<'static>> = Vec::new();

    // Tab strip for batched questions: chips for each question plus a trailing
    // Submit; the active one is highlighted.
    if nq > 1 {
        let mut spans: Vec<Span<'static>> = vec![Span::styled("< ", Style::default().fg(dim))];
        for (qi, q) in set.questions.iter().enumerate() {
            let label = truncate_to_cells(&q.header, 12);
            if qi == set.active {
                spans.push(chip(&label, theme.colors.background.to_color(), brand));
            } else {
                spans.push(Span::styled(
                    format!(" {} ", label),
                    Style::default().fg(dim),
                ));
            }
            spans.push(Span::raw(" "));
        }
        if set.active >= nq {
            spans.push(chip("Submit", theme.colors.background.to_color(), brand));
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
            push_wrapped(
                &mut lines,
                Line::from(Span::styled(
                    format!("- {}", q.question),
                    Style::default().fg(white),
                )),
                width,
                2,
            );
            let value = if ans.selected.is_empty() {
                "(no selection)".to_string()
            } else {
                ans.selected.join(", ")
            };
            push_wrapped(
                &mut lines,
                Line::from(Span::styled(
                    format!("   -> {}", value),
                    Style::default().fg(brand),
                )),
                width,
                6,
            );
        }
        if has_memory {
            let mark = if set.remember { "[x]" } else { "[ ]" };
            lines.push(Line::from(Span::styled(
                format!("{mark} Remember my answers across sessions (r)"),
                Style::default().fg(if set.remember { brand } else { dim }),
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
        let mut foot = String::from("Enter to select | Up/Down to navigate | c: chat");
        if has_memory {
            foot.push_str(" | r: remember");
        }
        foot.push_str(" | Esc to cancel");
        push_wrapped(
            &mut lines,
            Line::from(Span::styled(foot, Style::default().fg(dim))),
            width,
            0,
        );
        return lines;
    }

    // A single question tab.
    let q = &set.questions[set.active];
    let sel = &set.selections[set.active];

    // The batched tab strip already shows the header chip for the active
    // question, so only render it here (above the title) when there's no tab
    // strip — i.e. a single question. Otherwise it's a duplicate.
    if nq == 1 {
        lines.push(Line::from(chip(
            &truncate_to_cells(&q.header, 12),
            theme.colors.background.to_color(),
            brand,
        )));
    }
    push_wrapped(
        &mut lines,
        Line::from(Span::styled(
            q.question.clone(),
            Style::default().fg(white).add_modifier(Modifier::BOLD),
        )),
        width,
        0,
    );
    lines.push(Line::from(""));

    if q.is_input() {
        push_input_lines(&mut lines, q, sel, theme, width);
    } else if q.is_rank() {
        push_rank_lines(&mut lines, q, sel, theme, width);
    } else {
        push_choice_lines(&mut lines, q, sel, theme, width);
    }

    // Notes line (choice kinds only — input kinds capture every keystroke).
    if q.is_choice() {
        lines.push(Line::from(""));
        if set.editing_note {
            push_wrapped(
                &mut lines,
                Line::from(vec![
                    Span::styled("Notes: ", Style::default().fg(brand)),
                    Span::styled(sel.note.clone(), Style::default().fg(white)),
                    Span::styled("_", Style::default().fg(brand)),
                ]),
                width,
                7,
            );
        } else if !sel.note.trim().is_empty() {
            push_wrapped(
                &mut lines,
                Line::from(vec![
                    Span::styled("Notes: ", Style::default().fg(dim)),
                    Span::styled(sel.note.clone(), Style::default().fg(white)),
                ]),
                width,
                7,
            );
        } else {
            lines.push(Line::from(Span::styled(
                "Notes: press n to add notes",
                Style::default().fg(dim),
            )));
        }
    }

    // Footer hints, tailored to the kind.
    lines.push(Line::from(""));
    let mut hint = if q.is_input() {
        let mut h = String::from("Type to edit");
        if matches!(q.kind, mermaid_domain::QuestionKind::Number { .. }) {
            h.push_str(" | Up/Down to step");
        }
        h.push_str(" | Enter to submit");
        h
    } else if q.is_rank() {
        String::from("Up/Down to move | Space to grab | Enter to submit")
    } else {
        String::from("Enter to select | Up/Down to navigate")
    };
    if nq > 1 {
        hint.push_str(" | Tab to switch");
    }
    if q.is_choice() {
        hint.push_str(" | n: notes | c: chat");
        if has_memory {
            hint.push_str(" | r: remember");
        }
    }
    hint.push_str(" | Esc to cancel");
    push_wrapped(
        &mut lines,
        Line::from(Span::styled(hint, Style::default().fg(dim))),
        width,
        0,
    );

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
pub fn question_modal_height(set: &PendingQuestionSet, theme: &Theme, total_width: u16) -> u16 {
    let left = build_question_lines(set, theme, question_column_width(set, total_width)).len();
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
            Paragraph::new(build_question_lines(
                self.set,
                self.theme,
                question_column_width(self.set, self.width),
            ))
            .render(cols[0], buf);
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
            Paragraph::new(build_question_lines(
                self.set,
                self.theme,
                question_column_width(self.set, self.width),
            ))
            .render(inner, buf);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mermaid_domain::{Question, QuestionOption, ToolCallId, TurnId};

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
        assert_eq!(
            lines[0].spans[0].style.fg,
            Some(theme.colors.info.to_color())
        );
        assert_eq!(
            lines[1].spans[0].style.fg,
            Some(theme.colors.error.to_color())
        );
        assert_eq!(
            lines[2].spans[0].style.fg,
            Some(theme.colors.success.to_color())
        );
    }

    #[test]
    fn tall_preview_drives_modal_height() {
        let body = std::iter::repeat_n("line", 20)
            .collect::<Vec<_>>()
            .join("\n");
        let q = Question {
            header: "H".to_string(),
            question: "Q?".to_string(),
            kind: mermaid_domain::QuestionKind::Select,
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
            memory_key: None,
        };
        let set = PendingQuestionSet::new(TurnId(1), ToolCallId(1), vec![q]);
        let theme = Theme::dark();
        // Modal grows to fit the 20-line preview (plus the 2-line border).
        assert!(
            question_modal_height(&set, &theme, 120) >= 22,
            "expected height >= 22 to fit the preview"
        );
    }

    fn q_with_header(header: &str) -> Question {
        Question {
            header: header.to_string(),
            question: format!("Which {}?", header),
            kind: mermaid_domain::QuestionKind::Select,
            options: vec![opt_with_preview("A", None), opt_with_preview("B", None)],
            memory_key: None,
        }
    }

    /// Count lines that are a lone header chip (a single span whose text is
    /// exactly `label`). The tab strip renders the header among several spans, so
    /// it never matches; only the standalone chip above the title does.
    fn lone_chip_lines(lines: &[Line<'static>], label: &str) -> usize {
        // `chip()` pads the label to " label ", so trim before comparing.
        lines
            .iter()
            .filter(|l| l.spans.len() == 1 && l.spans[0].content.as_ref().trim() == label)
            .count()
    }

    #[test]
    fn header_chip_shown_once_for_single_question() {
        // No tab strip for a single question, so the header chip appears exactly
        // once (above the title) — otherwise the header would vanish entirely.
        let set = PendingQuestionSet::new(TurnId(1), ToolCallId(1), vec![q_with_header("HDR")]);
        let lines = build_question_lines(&set, &Theme::dark(), 120);
        assert_eq!(lone_chip_lines(&lines, "HDR"), 1);
    }

    #[test]
    fn header_chip_not_duplicated_above_title_when_batched() {
        // The batched tab strip already shows the active header, so there must be
        // NO standalone header chip above the title (it used to be duplicated).
        let set = PendingQuestionSet::new(
            TurnId(1),
            ToolCallId(1),
            vec![q_with_header("HDR"), q_with_header("Other")],
        );
        let lines = build_question_lines(&set, &Theme::dark(), 120);
        assert_eq!(lone_chip_lines(&lines, "HDR"), 0);
    }

    /// Regression: the modal built its lines width-blind, so a long option
    /// description ran under the right border and was clipped mid-word
    /// ("…screenshots + file dro"). A modal is a fixed box — every line it
    /// draws must fit, and the height estimator must count the wrapped lines.
    #[test]
    fn long_option_text_wraps_inside_the_modal_instead_of_being_clipped() {
        let desc = "Press Shift+Tab or run /safety ask so I can run PowerShell diagnostics and \
                    patch clipboard.rs to handle screenshots + file drops";
        let q = Question {
            header: "Safety".to_string(),
            question: "I'm in read_only mode and can't run diagnostics or patch the clipboard \
                       code. How should I proceed?"
                .to_string(),
            kind: mermaid_domain::QuestionKind::Select,
            options: vec![QuestionOption {
                label: "Switch to ask/full_access (Recommended)".to_string(),
                description: Some(desc.to_string()),
                preview: None,
                recommended: true,
            }],
            memory_key: None,
        };
        let set = PendingQuestionSet::new(TurnId(1), ToolCallId(1), vec![q]);
        let theme = Theme::dark();
        let width = 60usize;
        let lines = build_question_lines(&set, &theme, width);
        for line in &lines {
            let w: usize = line.spans.iter().map(|s| s.content.as_ref().width()).sum();
            assert!(
                w <= width,
                "line overflows the modal ({w} > {width}): {:?}",
                line.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            );
        }
        // Nothing is lost to the wrap: the description's tail still renders.
        let all: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(
            all.contains("file drops"),
            "the clipped tail must survive wrapping: {all}"
        );
        // The reserved height counts the wrapped lines, not the unwrapped ones.
        assert!(
            question_modal_height(&set, &theme, (width + 2) as u16) as usize >= lines.len() + 2,
            "the estimator must reserve room for every wrapped line"
        );
    }
}
