use crate::markdown::parse_markdown_inline;
use crate::node::{Line, Span, UiNode};
use crate::theme::{StyleToken, Theme, ThemeToken};
use crate::wrap::{truncate_to_cells, wrap_styled_line};
use mermaid_domain::checklist::{ChecklistItem, ChecklistStatus, ChecklistStore};
use mermaid_domain::{ChecklistOrigin, TurnState};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const MAX_ROWS: usize = 8;

#[must_use]
pub fn tasks_visible(
    store: &ChecklistStore,
    turn: &TurnState,
    collapsed: bool,
    attached: bool,
) -> bool {
    if store.is_empty() {
        return false;
    }
    if collapsed && !attached {
        return false;
    }
    !(matches!(turn, TurnState::Idle) && store.all_done())
}

#[must_use]
pub fn build_task_lines(
    store: &ChecklistStore,
    collapsed: bool,
    attached: bool,
    width: u16,
    theme: &Theme,
) -> Vec<Line> {
    if width < 10 {
        return Vec::new();
    }
    let width = width as usize;
    let meta_style = StyleToken::new().fg(ThemeToken::TextSecondary).dim();

    if collapsed {
        return vec![collapsed_line(store, width, theme, meta_style)];
    }

    let visible: Vec<&ChecklistItem> = store.visible().collect();
    let (start, end, hidden_completed, hidden_pending, hidden_blocked) =
        window_tasks(&visible, attached, width, theme, meta_style);

    let mut lines = Vec::new();
    for (i, task) in visible[start..end].iter().enumerate() {
        lines.extend(task_rows(task, i == 0, attached, width, theme, meta_style));
    }
    if hidden_completed + hidden_pending + hidden_blocked > 0 {
        let mut bits = Vec::new();
        if hidden_pending > 0 {
            bits.push(format!("+{hidden_pending} pending"));
        }
        if hidden_blocked > 0 {
            bits.push(format!("+{hidden_blocked} blocked"));
        }
        if hidden_completed > 0 {
            bits.push(format!("{hidden_completed} completed"));
        }
        let footer_pad = if attached { "    " } else { "  " };
        lines.push(Line::from(Span::styled(
            truncate_to_cells(&format!("{footer_pad}… {}", bits.join(", ")), width),
            meta_style,
        )));
    }
    lines
}

#[must_use]
pub fn tasks_height(
    store: &ChecklistStore,
    collapsed: bool,
    attached: bool,
    width: u16,
    theme: &Theme,
) -> u16 {
    if collapsed {
        return 1;
    }
    build_task_lines(store, collapsed, attached, width, theme).len() as u16
}

fn window_tasks(
    visible: &[&ChecklistItem],
    attached: bool,
    width: usize,
    theme: &Theme,
    meta_style: StyleToken,
) -> (usize, usize, usize, usize, usize) {
    if visible.is_empty() {
        return (0, 0, 0, 0, 0);
    }
    let per_task_lines: Vec<usize> = visible
        .iter()
        .enumerate()
        .map(|(i, t)| task_rows(t, i == 0, attached, width, theme, meta_style).len())
        .collect();
    let total_lines: usize = per_task_lines.iter().sum();

    if total_lines <= MAX_ROWS {
        return (0, visible.len(), 0, 0, 0);
    }

    let budget = MAX_ROWS.saturating_sub(1).max(1);
    let active = visible
        .iter()
        .position(|t| t.status != ChecklistStatus::Completed)
        .unwrap_or(0);

    let mut start = active;
    let mut end = (active + 1).min(visible.len());
    let mut used = per_task_lines[active];

    while end < visible.len() && used + per_task_lines[end] <= budget {
        used += per_task_lines[end];
        end += 1;
    }
    while start > 0 && used + per_task_lines[start - 1] <= budget {
        start -= 1;
        used += per_task_lines[start];
    }

    let before = &visible[..start];
    let after = &visible[end..];
    let count = |slice: &[&ChecklistItem], status: ChecklistStatus| {
        slice.iter().filter(|t| t.status == status).count()
    };
    (
        start,
        end,
        count(before, ChecklistStatus::Completed) + count(after, ChecklistStatus::Completed),
        count(before, ChecklistStatus::Pending)
            + count(after, ChecklistStatus::Pending)
            + count(before, ChecklistStatus::InProgress)
            + count(after, ChecklistStatus::InProgress),
        count(before, ChecklistStatus::Blocked) + count(after, ChecklistStatus::Blocked),
    )
}

fn task_rows(
    task: &ChecklistItem,
    first: bool,
    attached: bool,
    width: usize,
    theme: &Theme,
    meta_style: StyleToken,
) -> Vec<Line> {
    let gutter = match (attached, first) {
        (true, true) => " ⎿ ",
        (true, false) => "   ",
        (false, _) => "",
    };
    let brand = StyleToken::new().fg(ThemeToken::Brand);
    let warning = StyleToken::new().fg(ThemeToken::Warning);
    let text = StyleToken::new().fg(ThemeToken::TextPrimary);

    let (glyph, glyph_style, base_style) = match task.status {
        ChecklistStatus::Completed => {
            let mut s = meta_style;
            s.strikethrough = true;
            ("√ ", brand, s)
        },
        ChecklistStatus::InProgress => ("■ ", warning, brand.bold()),
        ChecklistStatus::Pending => ("□ ", meta_style, text),
        ChecklistStatus::Blocked => ("⊘ ", warning, text),
        ChecklistStatus::Deleted => ("x ", meta_style, meta_style),
    };

    let continuation_indent = gutter.width() + glyph.width();
    let gutter_span = Span::styled(gutter.to_string(), meta_style);
    let glyph_span = Span::styled(glyph.to_string(), glyph_style);

    let mut parsed = parse_markdown_inline(&task.subject, theme, base_style);
    if task.status == ChecklistStatus::Completed {
        for span in &mut parsed.spans {
            span.style.strikethrough = true;
            span.style.dim = true;
        }
    }

    let mut spans = vec![gutter_span, glyph_span];
    spans.extend(parsed.spans);

    if task.origin == ChecklistOrigin::User {
        let mut user_style = meta_style;
        if task.status == ChecklistStatus::Completed {
            user_style.strikethrough = true;
        }
        spans.push(Span::styled(" (you)".to_string(), user_style));
    }

    if task.status == ChecklistStatus::Completed {
        let suffix = cost_suffix(task);
        if !suffix.is_empty() {
            let mut s = meta_style;
            s.strikethrough = true;
            spans.push(Span::styled(suffix, s));
        }
    }

    wrap_styled_line(Line::from(spans), width, continuation_indent)
}

fn truncate_line_to_cells(line: Line, width: usize) -> Line {
    let total_w: usize = line.spans.iter().map(Span::width).sum();
    if total_w <= width {
        return line;
    }
    if width == 0 {
        return Line::from(Vec::new());
    }
    let budget = width.saturating_sub(1);
    let mut out_spans = Vec::new();
    let mut current_w = 0usize;
    let mut last_style = StyleToken::default();
    for span in line.spans {
        last_style = span.style;
        let mut buf = String::new();
        for ch in span.content.chars() {
            let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
            if current_w + cw > budget {
                break;
            }
            buf.push(ch);
            current_w += cw;
        }
        if !buf.is_empty() {
            out_spans.push(Span::styled(buf, span.style));
        }
        if current_w >= budget {
            break;
        }
    }
    out_spans.push(Span::styled("…", last_style));
    Line::from(out_spans)
}

fn collapsed_line(
    store: &ChecklistStore,
    width: usize,
    theme: &Theme,
    meta_style: StyleToken,
) -> Line {
    let text_style = StyleToken::new().fg(ThemeToken::TextPrimary);
    match store.next_pending() {
        Some(next) => {
            let head = " ⎿ Next: ";
            let head_span = Span::styled(head.to_string(), meta_style);
            let parsed = parse_markdown_inline(&next.subject, theme, text_style);
            let mut full_spans = vec![head_span];
            full_spans.extend(parsed.spans);
            truncate_line_to_cells(Line::from(full_spans), width)
        },
        None => Line::from(Span::styled(
            format!(" ⎿ {}", store.progress_string()),
            meta_style,
        )),
    }
}

fn cost_suffix(task: &ChecklistItem) -> String {
    let mut bits = Vec::new();
    if let Some(secs) = task.elapsed_secs()
        && secs > 0
    {
        bits.push(format_duration(secs));
    }
    if let Some(tokens) = task.tokens_spent
        && tokens > 0
    {
        bits.push(format!(
            "{} tok",
            mermaid_domain::format_compact_count(tokens as usize)
        ));
    }
    if bits.is_empty() {
        String::new()
    } else {
        format!(" ({})", bits.join(" · "))
    }
}

fn format_duration(secs: u64) -> String {
    if secs >= 3600 {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

#[must_use]
pub fn build_tasks_view(
    store: &ChecklistStore,
    collapsed: bool,
    attached: bool,
    width: u16,
    theme: &Theme,
) -> UiNode {
    let lines = build_task_lines(store, collapsed, attached, width, theme);
    UiNode::text(lines)
}
