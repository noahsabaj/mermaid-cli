//! The live task checklist, rendered directly under the status/spinner line
//! (Claude Code visual parity — the spinner row reads as the checklist
//! header, these rows hang beneath it behind a `⎿` gutter).
//!
//! Expanded (default): one row per task, windowed around the in-progress
//! task when the list outgrows the cap, with a dim overflow footer
//! ("… +4 pending, 2 completed"). Collapsed (Ctrl+T): a single "Next:" row
//! naming the upcoming pending task (the ACTIVE task already lives on the
//! spinner line above).
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

use crate::domain::tasks::{TaskItem, TaskStatus, TaskStore};
use crate::domain::{TaskOrigin, TurnState};
use crate::render::theme::Theme;

use super::truncate_to_cells;

/// Maximum task rows shown expanded (excluding the overflow footer).
const MAX_ROWS: usize = 8;

/// Whether the checklist band renders at all: there must be something to
/// show, and a fully-green list retires once the run goes idle (finished
/// work stays visible only while unfinished work remains). Collapsed with no
/// status widget above (`attached` false) renders nothing — the state
/// persists and the one-liner reappears when the next run starts.
pub fn tasks_visible(store: &TaskStore, turn: &TurnState, collapsed: bool, attached: bool) -> bool {
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
pub fn build_task_lines(
    store: &TaskStore,
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

    let visible: Vec<&TaskItem> = store.visible().collect();
    let (window, hidden_completed, hidden_pending, hidden_blocked) = window_rows(&visible);

    let mut lines = Vec::with_capacity(window.len() + 1);
    for (i, task) in window.iter().enumerate() {
        lines.push(task_row(task, i == 0, attached, width, theme, meta_style));
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
pub fn tasks_height(store: &TaskStore, collapsed: bool) -> u16 {
    if collapsed {
        return 1;
    }
    let visible = store.visible().count();
    if visible <= MAX_ROWS {
        visible as u16
    } else {
        // Windowed rows + overflow footer.
        (MAX_ROWS as u16) + 1
    }
}

/// Pick the rows to show: everything when it fits; otherwise start at the
/// first non-completed task (completed rows scroll away first, matching the
/// screenshots) and summarize the rest in the footer.
fn window_rows<'a>(visible: &[&'a TaskItem]) -> (Vec<&'a TaskItem>, usize, usize, usize) {
    if visible.len() <= MAX_ROWS {
        return (visible.to_vec(), 0, 0, 0);
    }
    let start = visible
        .iter()
        .position(|t| t.status != TaskStatus::Completed)
        .unwrap_or(0);
    // Keep the window from overshooting the tail: back it up so MAX_ROWS
    // always fill when enough tasks exist.
    let start = start.min(visible.len() - MAX_ROWS);
    let window: Vec<&TaskItem> = visible[start..start + MAX_ROWS].to_vec();
    let hidden = |slice: &[&TaskItem], status: TaskStatus| {
        slice.iter().filter(|t| t.status == status).count()
    };
    let before = &visible[..start];
    let after = &visible[start + MAX_ROWS..];
    (
        window,
        hidden(before, TaskStatus::Completed) + hidden(after, TaskStatus::Completed),
        hidden(before, TaskStatus::Pending)
            + hidden(after, TaskStatus::Pending)
            + hidden(before, TaskStatus::InProgress)
            + hidden(after, TaskStatus::InProgress),
        hidden(before, TaskStatus::Blocked) + hidden(after, TaskStatus::Blocked),
    )
}

/// One checklist row. Attached, the first row carries the `⎿` gutter that
/// visually connects the list to the spinner line above and the rest indent
/// to align; detached there is nothing to hang from, so rows sit flush-left.
fn task_row(
    task: &TaskItem,
    first: bool,
    attached: bool,
    width: usize,
    theme: &Theme,
    meta_style: Style,
) -> Line<'static> {
    let gutter = match (attached, first) {
        (true, true) => " ⎿ ",
        (true, false) => "   ",
        (false, _) => "",
    };
    let brand = Style::new().fg(theme.colors.brand.to_color());
    let warning = Style::new().fg(theme.colors.warning.to_color());
    let text = Style::new().fg(theme.colors.text_primary.to_color());

    // Completed rows earn a dim cost suffix when stamps exist: "(2m 10s · 8.4k tok)".
    let suffix = if task.status == TaskStatus::Completed {
        cost_suffix(task)
    } else {
        String::new()
    };
    let user_marker = if task.origin == TaskOrigin::User {
        " (you)"
    } else {
        ""
    };

    let budget = width
        .saturating_sub(gutter.len() + 2) // glyph + space
        .saturating_sub(suffix.len())
        .saturating_sub(user_marker.len());
    let subject = truncate_to_cells(&task.subject, budget.max(4));

    let mut spans = vec![Span::styled(gutter.to_string(), meta_style)];
    match task.status {
        TaskStatus::Completed => {
            spans.push(Span::styled("√ ", brand));
            spans.push(Span::styled(subject, meta_style.crossed_out()));
        },
        TaskStatus::InProgress => {
            spans.push(Span::styled("■ ", warning));
            spans.push(Span::styled(subject, brand.bold()));
        },
        TaskStatus::Pending => {
            spans.push(Span::styled("□ ", meta_style));
            spans.push(Span::styled(subject, text));
        },
        // ⊘ (U+2298, Mathematical Operators) stays outside the banned emoji
        // ranges like the other glyphs.
        TaskStatus::Blocked => {
            spans.push(Span::styled("⊘ ", warning));
            spans.push(Span::styled(subject, text));
        },
        // Deleted never reaches here (filtered by `visible()`), but the
        // match stays exhaustive per house rule.
        TaskStatus::Deleted => {
            spans.push(Span::styled("x ", meta_style));
            spans.push(Span::styled(subject, meta_style));
        },
    }
    if !user_marker.is_empty() {
        spans.push(Span::styled(user_marker.to_string(), meta_style));
    }
    if !suffix.is_empty() {
        spans.push(Span::styled(suffix, meta_style));
    }
    Line::from(spans)
}

/// The collapsed one-liner: what's up next (the active task already shows on
/// the spinner line). All done → the plain progress count.
fn collapsed_line(
    store: &TaskStore,
    width: usize,
    theme: &Theme,
    meta_style: Style,
) -> Line<'static> {
    let text_style = Style::new().fg(theme.colors.text_primary.to_color());
    match store.next_pending() {
        Some(next) => {
            let head = " ⎿ Next: ";
            let budget = (width).saturating_sub(head.len()).max(4);
            Line::from(vec![
                Span::styled(head.to_string(), meta_style),
                Span::styled(truncate_to_cells(&next.subject, budget), text_style),
            ])
        },
        None => Line::from(Span::styled(
            format!(" ⎿ {}", store.progress_string()),
            meta_style,
        )),
    }
}

/// `" (2m 10s · 8.4k tok)"` for a completed task, empty when unstamped.
fn cost_suffix(task: &TaskItem) -> String {
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
            crate::domain::format_compact_count(tokens as usize)
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
    use crate::domain::TaskEdit;
    use crate::domain::tasks::{Stamp, TaskSpec};

    fn store_of(statuses: &[TaskStatus]) -> TaskStore {
        let mut store = TaskStore::default();
        store.create(
            statuses
                .iter()
                .enumerate()
                .map(|(i, _)| TaskSpec {
                    subject: format!("task number {i}"),
                    active_form: format!("doing task {i}"),
                    description: None,
                    in_progress: false,
                })
                .collect(),
            TaskOrigin::Model,
            Stamp::default(),
        );
        let edits: Vec<TaskEdit> = statuses
            .iter()
            .enumerate()
            .filter(|(_, s)| **s != TaskStatus::Pending)
            .map(|(i, s)| TaskEdit {
                id: (i + 1) as u32,
                status: Some(*s),
                ..TaskEdit::default()
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
        use TaskStatus::*;
        let store = store_of(&[Completed, InProgress, Pending]);
        assert!(tasks_visible(&store, &TurnState::Idle, false, false));
        let done = store_of(&[Completed, Completed]);
        assert!(
            !tasks_visible(&done, &TurnState::Idle, false, true),
            "all-done idle retires"
        );
        assert!(!tasks_visible(
            &TaskStore::default(),
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
        use TaskStatus::*;
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
        use TaskStatus::*;
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
        use TaskStatus::*;
        let statuses: Vec<TaskStatus> = [Completed, Completed]
            .into_iter()
            .chain([InProgress])
            .chain(std::iter::repeat_n(Pending, 9))
            .collect();
        let store = store_of(&statuses);
        let lines = build_task_lines(&store, false, true, 80, &Theme::dark());
        let rows = rendered(&lines);
        // 8 windowed rows + footer.
        assert_eq!(rows.len(), 9);
        assert!(
            rows[0].contains("■"),
            "window starts at in_progress: {:?}",
            rows[0]
        );
        let footer = rows.last().unwrap();
        assert!(footer.contains("+2 pending"), "{footer:?}");
        assert!(footer.contains("2 completed"), "{footer:?}");
        assert_eq!(tasks_height(&store, false), 9);
    }

    #[test]
    fn blocked_rows_render_glyph_and_footer_counts_them() {
        use TaskStatus::*;
        let store = store_of(&[Blocked, InProgress, Pending]);
        let lines = build_task_lines(&store, false, true, 80, &Theme::dark());
        let rows = rendered(&lines);
        assert!(rows[0].starts_with(" ⎿ ⊘ "), "{:?}", rows[0]);

        // A blocked task hidden past the window shows up in the footer.
        let statuses: Vec<TaskStatus> = [InProgress]
            .into_iter()
            .chain(std::iter::repeat_n(Pending, 7))
            .chain([Blocked])
            .chain(std::iter::repeat_n(Pending, 2))
            .collect();
        let store = store_of(&statuses);
        let lines = build_task_lines(&store, false, true, 80, &Theme::dark());
        let footer = rendered(&lines).last().unwrap().clone();
        assert!(footer.contains("+1 blocked"), "{footer:?}");
        assert!(footer.contains("+2 pending"), "{footer:?}");
    }

    #[test]
    fn collapsed_shows_next_pending() {
        use TaskStatus::*;
        let store = store_of(&[Completed, InProgress, Pending]);
        let lines = build_task_lines(&store, true, true, 80, &Theme::dark());
        let rows = rendered(&lines);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].contains("Next: task number 2"), "{:?}", rows[0]);
        assert_eq!(tasks_height(&store, true), 1);

        let no_pending = store_of(&[Completed, InProgress]);
        let rows = rendered(&build_task_lines(
            &no_pending,
            true,
            true,
            80,
            &Theme::dark(),
        ));
        assert!(rows[0].contains("Tasks 1/2"), "{:?}", rows[0]);
    }

    #[test]
    fn completed_rows_show_cost_and_user_marker() {
        let mut store = TaskStore::default();
        store.create(
            vec![TaskSpec {
                subject: "review the docs".into(),
                active_form: "reviewing the docs".into(),
                description: None,
                in_progress: true,
            }],
            TaskOrigin::User,
            Stamp {
                now_epoch: 100,
                run_tokens: 1_000,
            },
        );
        store.apply(
            &[TaskEdit {
                id: 1,
                status: Some(TaskStatus::Completed),
                ..TaskEdit::default()
            }],
            Stamp {
                now_epoch: 230,
                run_tokens: 9_400,
            },
        );
        // A single completed task while a run is still busy stays visible.
        let lines = build_task_lines(&store, false, true, 100, &Theme::dark());
        let row = &rendered(&lines)[0];
        assert!(row.contains("(2m 10s · 8.4k tok)"), "{row:?}");
        assert!(row.contains("(you)"), "{row:?}");
    }
}
