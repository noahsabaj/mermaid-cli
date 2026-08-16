use unicode_width::UnicodeWidthStr;

use crate::node::{BorderStyle, Constraint, Line, Span, UiNode};
use crate::theme::{StyleToken, Theme, ThemeToken};
use crate::wrap::{truncate_to_cells, wrap_styled_line};
use mermaid_model::question::{OptionPreview, PendingQuestionSet, Question, QuestionSelection};

fn push_wrapped(lines: &mut Vec<Line>, line: Line, width: usize, hang: usize) {
    lines.extend(wrap_styled_line(
        line,
        width,
        hang.min(width.saturating_sub(1)),
    ));
}

fn chip(label: &str) -> Span {
    Span::styled(
        format!(" {label} "),
        StyleToken::new()
            .fg(ThemeToken::Background)
            .bg(ThemeToken::Brand)
            .bold(),
    )
}

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

fn push_option_item(
    lines: &mut Vec<Line>,
    opt: &mermaid_model::question::QuestionOption,
    idx: usize,
    focused: bool,
    checked: bool,
    multi: bool,
    width: usize,
) {
    let mut spans: Vec<Span> = vec![
        Span::styled(
            if focused { "> " } else { "  " },
            StyleToken::new().fg(ThemeToken::Brand),
        ),
        Span::styled(
            format!("{}. ", idx + 1),
            StyleToken::new().fg(ThemeToken::TextDisabled),
        ),
    ];
    if multi {
        spans.push(Span::styled(
            if checked { "[x] " } else { "[ ] " },
            StyleToken::new().fg(if checked {
                ThemeToken::Brand
            } else {
                ThemeToken::TextDisabled
            }),
        ));
    }
    let label_style = if focused {
        StyleToken::new().fg(ThemeToken::Brand).bold()
    } else if !multi && checked {
        StyleToken::new().fg(ThemeToken::Brand)
    } else {
        StyleToken::new().fg(ThemeToken::TextPrimary).bold()
    };
    let hang: usize = spans.iter().map(Span::width).sum();
    spans.push(Span::styled(opt.label.clone(), label_style));
    push_wrapped(lines, Line::from(spans), width, hang);
    if let Some(desc) = &opt.description {
        push_wrapped(
            lines,
            Line::from(Span::styled(
                format!("     {desc}"),
                StyleToken::new().fg(ThemeToken::TextDisabled),
            )),
            width,
            5,
        );
    }
}

fn push_other_row(
    lines: &mut Vec<Line>,
    n: usize,
    sel: &QuestionSelection,
    multi: bool,
    width: usize,
) {
    let other_focused = sel.cursor == n;
    let mut spans: Vec<Span> = vec![
        Span::styled(
            if other_focused { "> " } else { "  " },
            StyleToken::new().fg(ThemeToken::Brand),
        ),
        Span::styled(
            format!("{}. ", n + 1),
            StyleToken::new().fg(ThemeToken::TextDisabled),
        ),
    ];
    if multi {
        let checked = !sel.other_text.trim().is_empty();
        spans.push(Span::styled(
            if checked { "[x] " } else { "[ ] " },
            StyleToken::new().fg(if checked {
                ThemeToken::Brand
            } else {
                ThemeToken::TextDisabled
            }),
        ));
    }
    let hang: usize = spans.iter().map(Span::width).sum();
    if sel.other_text.is_empty() {
        spans.push(Span::styled(
            "Type something",
            StyleToken::new().fg(ThemeToken::TextDisabled),
        ));
    } else {
        spans.push(Span::styled(
            sel.other_text.clone(),
            StyleToken::new().fg(ThemeToken::TextPrimary),
        ));
    }
    push_wrapped(lines, Line::from(spans), width, hang);
}

fn push_choice_lines(lines: &mut Vec<Line>, q: &Question, sel: &QuestionSelection, width: usize) {
    let n = q.options.len();
    let multi = q.is_multi();
    const MAX_VISIBLE: usize = 8;
    let (start, end) = option_window(sel.cursor, n, MAX_VISIBLE);
    if start > 0 {
        lines.push(Line::from(Span::styled(
            format!("  ... {start} more above"),
            StyleToken::new().fg(ThemeToken::TextDisabled),
        )));
    }
    for i in start..end {
        let opt = &q.options[i];
        let focused = sel.cursor == i;
        let checked = sel.chosen.contains(&i);
        push_option_item(lines, opt, i, focused, checked, multi, width);
    }
    if end < n {
        lines.push(Line::from(Span::styled(
            format!("  ... {} more below", n - end),
            StyleToken::new().fg(ThemeToken::TextDisabled),
        )));
    }

    push_other_row(lines, n, sel, multi, width);

    if multi {
        let focused = sel.cursor == n + 1;
        let style = if focused {
            StyleToken::new().fg(ThemeToken::Brand).bold()
        } else {
            StyleToken::new().fg(ThemeToken::TextPrimary).bold()
        };
        lines.push(Line::from(vec![
            Span::styled(
                if focused { "> " } else { "  " },
                StyleToken::new().fg(ThemeToken::Brand),
            ),
            Span::styled("Submit", style),
        ]));
    }
}

fn push_input_lines(
    lines: &mut Vec<Line>,
    q: &Question,
    sel: &QuestionSelection,
    max_width: usize,
) {
    let value = &sel.value;
    let mut field: Vec<Span> = vec![Span::styled("> ", StyleToken::new().fg(ThemeToken::Brand))];
    if value.is_empty() {
        field.push(Span::styled(
            input_placeholder(&q.kind),
            StyleToken::new().fg(ThemeToken::TextDisabled),
        ));
    } else {
        field.push(Span::styled(
            value.clone(),
            StyleToken::new().fg(ThemeToken::TextPrimary),
        ));
    }
    field.push(Span::styled("_", StyleToken::new().fg(ThemeToken::Brand)));
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
            StyleToken::new().fg(ThemeToken::Brand),
        )));
    }

    match mermaid_domain::validate_input(&q.kind, value) {
        Err(e) => push_wrapped(
            lines,
            Line::from(Span::styled(
                format!("  {e}"),
                StyleToken::new().fg(ThemeToken::Error),
            )),
            max_width,
            2,
        ),
        Ok(()) => lines.push(Line::from(Span::styled(
            "  Enter to submit",
            StyleToken::new().fg(ThemeToken::TextDisabled),
        ))),
    }
}

fn push_rank_lines(lines: &mut Vec<Line>, q: &Question, sel: &QuestionSelection, width: usize) {
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
            StyleToken::new().fg(ThemeToken::Brand).bold()
        } else {
            StyleToken::new().fg(ThemeToken::TextPrimary).bold()
        };
        let marker = format!("{}. ", pos + 1);
        let hang = prefix.width() + marker.width();
        push_wrapped(
            lines,
            Line::from(vec![
                Span::styled(prefix, StyleToken::new().fg(ThemeToken::Brand)),
                Span::styled(marker, StyleToken::new().fg(ThemeToken::TextDisabled)),
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

fn push_tab_strip(lines: &mut Vec<Line>, set: &PendingQuestionSet, nq: usize) {
    if nq <= 1 {
        return;
    }
    let mut spans: Vec<Span> = vec![Span::styled(
        "< ",
        StyleToken::new().fg(ThemeToken::TextDisabled),
    )];
    for (qi, q) in set.questions.iter().enumerate() {
        let label = truncate_to_cells(&q.header, 12);
        if qi == set.active {
            spans.push(chip(&label));
        } else {
            spans.push(Span::styled(
                format!(" {label} "),
                StyleToken::new().fg(ThemeToken::TextDisabled),
            ));
        }
        spans.push(Span::raw(" "));
    }
    if set.active >= nq {
        spans.push(chip("Submit"));
    } else {
        spans.push(Span::styled(
            " Submit ",
            StyleToken::new().fg(ThemeToken::TextDisabled),
        ));
    }
    spans.push(Span::styled(
        " >",
        StyleToken::new().fg(ThemeToken::TextDisabled),
    ));
    lines.push(Line::from(spans));
    lines.push(Line::from(""));
}

fn build_review_lines(set: &PendingQuestionSet, width: usize, has_memory: bool) -> Vec<Line> {
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        "Review your answers",
        StyleToken::new().fg(ThemeToken::TextPrimary).bold(),
    )));
    lines.push(Line::from(""));
    for (q, ans) in set.questions.iter().zip(set.build_answers()) {
        push_wrapped(
            &mut lines,
            Line::from(Span::styled(
                format!("- {}", q.question),
                StyleToken::new().fg(ThemeToken::TextPrimary),
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
                format!("   -> {value}"),
                StyleToken::new().fg(ThemeToken::Brand),
            )),
            width,
            6,
        );
    }
    if has_memory {
        let mark = if set.remember { "[x]" } else { "[ ]" };
        lines.push(Line::from(Span::styled(
            format!("{mark} Remember my answers across sessions (r)"),
            StyleToken::new().fg(if set.remember {
                ThemeToken::Brand
            } else {
                ThemeToken::TextDisabled
            }),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Ready to submit your answers?",
        StyleToken::new().fg(ThemeToken::TextDisabled),
    )));
    for (i, opt) in ["1. Submit answers", "2. Cancel"].iter().enumerate() {
        let focused = set.review_cursor == i;
        let style = if focused {
            StyleToken::new().fg(ThemeToken::Brand).bold()
        } else {
            StyleToken::new().fg(ThemeToken::TextPrimary)
        };
        lines.push(Line::from(vec![
            Span::styled(
                if focused { "> " } else { "  " },
                StyleToken::new().fg(ThemeToken::Brand),
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
        Line::from(Span::styled(
            foot,
            StyleToken::new().fg(ThemeToken::TextDisabled),
        )),
        width,
        0,
    );
    lines
}

#[must_use]
pub fn build_question_lines(set: &PendingQuestionSet, _theme: &Theme, width: usize) -> Vec<Line> {
    let nq = set.questions.len();
    let has_memory = set.questions.iter().any(|q| q.memory_key.is_some());
    let mut lines: Vec<Line> = Vec::new();

    push_tab_strip(&mut lines, set, nq);

    if set.active >= nq {
        return build_review_lines(set, width, has_memory);
    }

    let q = &set.questions[set.active];
    let sel = &set.selections[set.active];

    if nq == 1 {
        lines.push(Line::from(chip(&truncate_to_cells(&q.header, 12))));
    }
    push_wrapped(
        &mut lines,
        Line::from(Span::styled(
            q.question.clone(),
            StyleToken::new().fg(ThemeToken::TextPrimary).bold(),
        )),
        width,
        0,
    );
    lines.push(Line::from(""));

    if q.is_input() {
        push_input_lines(&mut lines, q, sel, width);
    } else if q.is_rank() {
        push_rank_lines(&mut lines, q, sel, width);
    } else {
        push_choice_lines(&mut lines, q, sel, width);
    }

    if q.is_choice() {
        lines.push(Line::from(""));
        if set.editing_note {
            push_wrapped(
                &mut lines,
                Line::from(vec![
                    Span::styled("Notes: ", StyleToken::new().fg(ThemeToken::Brand)),
                    Span::styled(
                        sel.note.clone(),
                        StyleToken::new().fg(ThemeToken::TextPrimary),
                    ),
                    Span::styled("_", StyleToken::new().fg(ThemeToken::Brand)),
                ]),
                width,
                7,
            );
        } else if !sel.note.trim().is_empty() {
            push_wrapped(
                &mut lines,
                Line::from(vec![
                    Span::styled("Notes: ", StyleToken::new().fg(ThemeToken::TextDisabled)),
                    Span::styled(
                        sel.note.clone(),
                        StyleToken::new().fg(ThemeToken::TextPrimary),
                    ),
                ]),
                width,
                7,
            );
        } else {
            lines.push(Line::from(Span::styled(
                "Notes: press n to add notes",
                StyleToken::new().fg(ThemeToken::TextDisabled),
            )));
        }
    }

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
        Line::from(Span::styled(
            hint,
            StyleToken::new().fg(ThemeToken::TextDisabled),
        )),
        width,
        0,
    );

    lines
}

pub fn question_column_width(set: &PendingQuestionSet, total_width: u16) -> usize {
    let inner = total_width.saturating_sub(2) as usize;
    if active_question_has_preview(set) {
        inner * 48 / 100
    } else {
        inner
    }
    .max(8)
}

pub fn focused_preview(set: &PendingQuestionSet) -> Option<&OptionPreview> {
    if set.active >= set.questions.len() {
        return None;
    }
    let q = &set.questions[set.active];
    let cursor = set.selections[set.active].cursor;
    q.options.get(cursor).and_then(|o| o.preview.as_ref())
}

pub fn active_question_has_preview(set: &PendingQuestionSet) -> bool {
    set.active < set.questions.len()
        && set.questions[set.active]
            .options
            .iter()
            .any(|o| o.preview.is_some())
}

pub fn max_preview_lines(set: &PendingQuestionSet) -> usize {
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

pub fn build_preview_lines(preview: &OptionPreview) -> Vec<Line> {
    preview
        .content
        .lines()
        .map(|raw| {
            let style = if preview.diff {
                match raw.chars().next() {
                    Some('+') => StyleToken::new().fg(ThemeToken::Success),
                    Some('-') => StyleToken::new().fg(ThemeToken::Error),
                    Some('@') => StyleToken::new().fg(ThemeToken::Info),
                    _ => StyleToken::new().fg(ThemeToken::CodeForeground),
                }
            } else {
                StyleToken::new().fg(ThemeToken::CodeForeground)
            };
            Line::from(Span::styled(raw.to_string(), style))
        })
        .collect()
}

#[must_use]
pub fn question_modal_height(set: &PendingQuestionSet, theme: &Theme, total_width: u16) -> u16 {
    let left = build_question_lines(set, theme, question_column_width(set, total_width)).len();
    let content = if active_question_has_preview(set) {
        left.max(max_preview_lines(set))
    } else {
        left
    };
    (content as u16).saturating_add(2)
}

#[must_use]
pub fn build_question_modal_view(
    set: &PendingQuestionSet,
    theme: &Theme,
    total_width: u16,
) -> UiNode {
    let q_lines = build_question_lines(set, theme, question_column_width(set, total_width));

    if active_question_has_preview(set) {
        let preview_lines = focused_preview(set)
            .map(build_preview_lines)
            .unwrap_or_default();

        let left_pane = UiNode::vertical(vec![UiNode::text(q_lines)], vec![]);
        let right_pane = UiNode::vertical(vec![UiNode::text(preview_lines)], vec![])
            .with_border(BorderStyle::Plain, Some(ThemeToken::TextDisabled));

        UiNode::horizontal(
            vec![left_pane, right_pane],
            vec![Constraint::Percentage(48), Constraint::Percentage(52)],
        )
        .with_border(BorderStyle::Plain, Some(ThemeToken::Brand))
    } else {
        UiNode::vertical(vec![UiNode::text(q_lines)], vec![])
            .with_border(BorderStyle::Plain, Some(ThemeToken::Brand))
    }
}
