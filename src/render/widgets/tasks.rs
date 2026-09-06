//! The live task checklist, rendered directly under the status/spinner line
//! (Claude Code visual parity — the spinner row reads as the checklist
//! header, these rows hang beneath it behind a `⎿` gutter).
//!
//! Expanded (default): wrapped rows per task with inline Markdown support,
//! windowed around the in-progress task when the list outgrows the visual line
//! cap, with a dim overflow footer ("… +4 pending, 2 completed"). Collapsed
//! (Ctrl+T): a single "Next:" row naming the upcoming pending task (the ACTIVE
//! task already lives on the spinner line above).
//!
//! The `⎿` gutter is subordinate to the status widget: it only renders when
//! the status zone above is actually showing (`attached`). Detached (idle,
//! no agent rows), expanded rows sit flush-left with no elbow, and the
//! collapsed one-liner disappears entirely — collapse is a minimize-while-
//! working affordance with nothing to minimize into when idle.
//!
//! Glyphs are deliberately outside the no-emoji CI ranges: `√` completed
//! (U+221A — the dingbat checkmarks U+2713/14 are banned), `■` in-progress,
//! `□` pending (geometric shapes), `⎿` gutter (house transcript glyph).

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::render::markdown::parse_markdown_inline;
use crate::render::theme::Theme;
use crate::render::wrap::wrap_styled_line;
use mermaid_domain::checklist::{ChecklistItem, ChecklistStatus, ChecklistStore};
use mermaid_domain::{ChecklistOrigin, TurnState};

use super::{truncate_line_to_cells, truncate_to_cells};

/// Maximum visual lines shown expanded (including task rows and overflow footer).
const MAX_ROWS: usize = 8;

/// Whether the checklist band renders at all: there must be something to
/// show, and a fully-green list retires once the run goes idle (finished
/// work stays visible only while unfinished work remains). Collapsed with no
/// status widget above (`attached` false) renders nothing — the state
/// persists and the one-liner reappears when the next run starts.
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

/// Build the checklist rows for the reserved zone. `collapsed` is the Ctrl+T
/// one-line form. `attached` means the status zone renders above, so the
/// first row carries the `⎿` connector; detached rows sit flush-left.
/// `width` is the zone's inner width in cells.
#[must_use]
pub fn build_task_lines(
    store: &ChecklistStore,
    collapsed: bool,
    attached: bool,
    width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    if width < 10 {
        return Vec::new();
    }
    let width = width as usize;
    let meta_style = Style::new()
        .fg(theme.colors.text_secondary.to_color())
        .dim();

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
        // Ellipsis aligns under the subject column in either mode.
        let footer_pad = if attached { "    " } else { "  " };
        lines.push(Line::from(Span::styled(
            truncate_to_cells(&format!("{footer_pad}… {}", bits.join(", ")), width),
            meta_style,
        )));
    }
    lines
}

/// Height the layout should reserve for the checklist zone.
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

/// Window tasks so total visual lines fit within [`MAX_ROWS`], centering on the
/// in-progress task and summarizing hidden items.
fn window_tasks(
    visible: &[&ChecklistItem],
    attached: bool,
    width: usize,
    theme: &Theme,
    meta_style: Style,
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

/// One checklist row wrapped across lines with inline Markdown and hanging indent.
fn task_rows(
    task: &ChecklistItem,
    first: bool,
    attached: bool,
    width: usize,
    theme: &Theme,
    meta_style: Style,
) -> Vec<Line<'static>> {
    let gutter = match (attached, first) {
        (true, true) => " ⎿ ",
        (true, false) => "   ",
        (false, _) => "",
    };
    let brand = Style::new().fg(theme.colors.brand.to_color());
    let warning = Style::new().fg(theme.colors.warning.to_color());
    let text = Style::new().fg(theme.colors.text_primary.to_color());

    let (glyph, glyph_style, base_style) = match task.status {
        ChecklistStatus::Completed => ("√ ", brand, meta_style.crossed_out()),
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
            span.style = span.style.crossed_out().dim();
        }
    }

    let mut spans = vec![gutter_span, glyph_span];
    spans.extend(parsed.spans);

    if task.origin == ChecklistOrigin::User {
        let user_style = if task.status == ChecklistStatus::Completed {
            meta_style.crossed_out()
        } else {
            meta_style
        };
        spans.push(Span::styled(" (you)".to_string(), user_style));
    }

    if task.status == ChecklistStatus::Completed {
        let suffix = cost_suffix(task);
        if !suffix.is_empty() {
            spans.push(Span::styled(suffix, meta_style.crossed_out()));
        }
    }

    wrap_styled_line(Line::from(spans), width, continuation_indent)
}

/// The collapsed one-liner: what's up next with inline Markdown parsing.
fn collapsed_line(
    store: &ChecklistStore,
    width: usize,
    theme: &Theme,
    meta_style: Style,
) -> Line<'static> {
    let text_style = Style::new().fg(theme.colors.text_primary.to_color());
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

/// `" (2m 10s · 8.4k tok)"` for a completed task, empty when unstamped.
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

#[cfg(test)]
mod tests {
    use super::*;
    use mermaid_domain::ChecklistEdit;
    use mermaid_domain::checklist::{ChecklistSpec, Stamp};
    use ratatui::style::Modifier;

    fn store_of(statuses: &[ChecklistStatus]) -> ChecklistStore {
        let mut store = ChecklistStore::default();
        store.create(
            statuses
                .iter()
                .enumerate()
                .map(|(i, _)| ChecklistSpec {
                    subject: format!("task number {i}"),
                    active_form: format!("doing task {i}"),
                    description: None,
                    in_progress: false,
                })
                .collect(),
            ChecklistOrigin::Model,
            Stamp::default(),
        );
        let edits: Vec<ChecklistEdit> = statuses
            .iter()
            .enumerate()
            .filter(|(_, s)| **s != ChecklistStatus::Pending)
            .map(|(i, s)| ChecklistEdit {
                id: (i + 1) as u32,
                status: Some(*s),
                ..ChecklistEdit::default()
            })
            .collect();
        store.apply(&edits, Stamp::default());
        store
    }

    fn rendered(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn visibility_rules() {
        use ChecklistStatus::*;
        let store = store_of(&[Completed, InProgress, Pending]);
        assert!(tasks_visible(&store, &TurnState::Idle, false, false));
        let done = store_of(&[Completed, Completed]);
        assert!(
            !tasks_visible(&done, &TurnState::Idle, false, true),
            "all-done idle retires"
        );
        assert!(!tasks_visible(
            &ChecklistStore::default(),
            &TurnState::Idle,
            false,
            true
        ));
        // Collapsed with no status widget above renders nothing at all…
        assert!(!tasks_visible(&store, &TurnState::Idle, true, false));
        // …but stays visible while attached (spinner or agent rows above).
        assert!(tasks_visible(&store, &TurnState::Idle, true, true));
    }

    #[test]
    fn expanded_rows_carry_glyphs_and_gutter() {
        use ChecklistStatus::*;
        let store = store_of(&[Completed, InProgress, Pending]);
        let lines = build_task_lines(&store, false, true, 80, &Theme::dark());
        let rows = rendered(&lines);
        assert_eq!(rows.len(), 3);
        assert!(rows[0].starts_with(" ⎿ √ "), "{:?}", rows[0]);
        assert!(rows[1].starts_with("   ■ "), "{:?}", rows[1]);
        assert!(rows[2].starts_with("   □ "), "{:?}", rows[2]);
    }

    #[test]
    fn detached_rows_drop_elbow_and_sit_flush() {
        use ChecklistStatus::*;
        let store = store_of(&[Completed, InProgress, Pending]);
        let lines = build_task_lines(&store, false, false, 80, &Theme::dark());
        let rows = rendered(&lines);
        assert_eq!(rows.len(), 3);
        assert!(rows[0].starts_with("√ "), "{:?}", rows[0]);
        assert!(rows[1].starts_with("■ "), "{:?}", rows[1]);
        assert!(rows[2].starts_with("□ "), "{:?}", rows[2]);
        assert!(!rows.iter().any(|r| r.contains('⎿')), "{rows:?}");
    }

    #[test]
    fn long_lists_window_and_summarize() {
        use ChecklistStatus::*;
        let statuses: Vec<ChecklistStatus> = [Completed, Completed]
            .into_iter()
            .chain([InProgress])
            .chain(std::iter::repeat_n(Pending, 9))
            .collect();
        let store = store_of(&statuses);
        let theme = Theme::dark();
        let lines = build_task_lines(&store, false, true, 80, &theme);
        let rows = rendered(&lines);
        // 8 windowed rows + footer.
        assert_eq!(rows.len(), 8);
        assert!(
            rows[0].contains("■"),
            "window starts at in_progress: {:?}",
            rows[0]
        );
        let footer = rows.last().unwrap();
        assert!(footer.contains("+3 pending"), "{footer:?}");
        assert!(footer.contains("2 completed"), "{footer:?}");
        assert_eq!(tasks_height(&store, false, true, 80, &theme), 8);
    }

    #[test]
    fn blocked_rows_render_glyph_and_footer_counts_them() {
        use ChecklistStatus::*;
        let store = store_of(&[Blocked, InProgress, Pending]);
        let lines = build_task_lines(&store, false, true, 80, &Theme::dark());
        let rows = rendered(&lines);
        assert!(rows[0].starts_with(" ⎿ ⊘ "), "{:?}", rows[0]);

        // A blocked task hidden past the window shows up in the footer.
        let statuses: Vec<ChecklistStatus> = [InProgress]
            .into_iter()
            .chain(std::iter::repeat_n(Pending, 7))
            .chain([Blocked])
            .chain(std::iter::repeat_n(Pending, 2))
            .collect();
        let store = store_of(&statuses);
        let lines = build_task_lines(&store, false, true, 80, &Theme::dark());
        let footer = rendered(&lines).last().unwrap().clone();
        assert!(footer.contains("+1 blocked"), "{footer:?}");
        assert!(footer.contains("+3 pending"), "{footer:?}");
    }

    #[test]
    fn collapsed_shows_next_pending() {
        use ChecklistStatus::*;
        let store = store_of(&[Completed, InProgress, Pending]);
        let theme = Theme::dark();
        let lines = build_task_lines(&store, true, true, 80, &theme);
        let rows = rendered(&lines);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].contains("Next: task number 2"), "{:?}", rows[0]);
        assert_eq!(tasks_height(&store, true, true, 80, &theme), 1);

        let no_pending = store_of(&[Completed, InProgress]);
        let rows = rendered(&build_task_lines(&no_pending, true, true, 80, &theme));
        assert!(rows[0].contains("Tasks 1/2"), "{:?}", rows[0]);
    }

    #[test]
    fn completed_rows_show_cost_and_user_marker() {
        let mut store = ChecklistStore::default();
        store.create(
            vec![ChecklistSpec {
                subject: "review the docs".into(),
                active_form: "reviewing the docs".into(),
                description: None,
                in_progress: true,
            }],
            ChecklistOrigin::User,
            Stamp {
                now_epoch: 100,
                run_tokens: 1_000,
            },
        );
        store.apply(
            &[ChecklistEdit {
                id: 1,
                status: Some(ChecklistStatus::Completed),
                ..ChecklistEdit::default()
            }],
            Stamp {
                now_epoch: 230,
                run_tokens: 9_400,
            },
        );
        let lines = build_task_lines(&store, false, true, 100, &Theme::dark());
        let row = &rendered(&lines)[0];
        assert!(row.contains("(2m 10s · 8.4k tok)"), "{row:?}");
        assert!(row.contains("(you)"), "{row:?}");
    }

    #[test]
    fn task_rows_parse_markdown_and_wrap_with_hanging_indent() {
        let mut store = ChecklistStore::default();
        store.create(
            vec![ChecklistSpec {
                subject: "**Add registry entries** - edit `crates/mermaid-model/src/models/providers.rs:205-333`".into(),
                active_form: "Adding registry entries".into(),
                description: None,
                in_progress: true,
            }],
            ChecklistOrigin::Model,
            Stamp::default(),
        );
        let theme = Theme::dark();
        // Width 40 will force wrapping
        let lines = build_task_lines(&store, false, true, 40, &theme);
        assert!(
            lines.len() >= 2,
            "should wrap into multiple lines: {lines:?}"
        );
        let rows = rendered(&lines);
        assert!(
            rows[0].starts_with(" ⎿ ■ Add registry entries"),
            "{:?}",
            rows[0]
        );
        // Continuation line should have hanging indent (5 spaces: 3 gutter + 2 glyph)
        assert!(
            rows[1].starts_with("     "),
            "continuation indent missing: {:?}",
            rows[1]
        );
        // Check that bold and code styling is present
        let bold_span = lines[0]
            .spans
            .iter()
            .find(|s| s.content == "Add")
            .expect("bold span present");
        assert!(bold_span.style.add_modifier.contains(Modifier::BOLD));
    }
}
