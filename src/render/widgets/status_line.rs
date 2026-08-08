use ratatui::style::Style;
use ratatui::text::{Line, Span};
use std::collections::VecDeque;
use unicode_width::UnicodeWidthStr;

use super::{GenerationStatus, truncate_to_cells};
use crate::render::theme::Theme;
use mermaid_domain::QueuedMessage;

/// How many queued-message rows to show under the spinner before stopping.
const MAX_QUEUED_ROWS: usize = 5;

/// How many live agent rows to show under the spinner before eliding.
const MAX_AGENT_ROWS: usize = 6;

/// One row of the live agent panel: a running (or backgrounded) subagent's
/// stable identity + activity. Built by the render layer from
/// `TurnState::ExecutingTools` + `live_tool_status` (+ the background-agent
/// registry); this widget only formats.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentPanelRow {
    pub description: String,
    /// Short stable label ("`read_file`…", "thinking"); empty = omitted.
    pub activity: String,
    /// Cumulative output-token estimate; 0 = omitted.
    pub tokens: usize,
    pub elapsed_secs: u64,
    /// True for agents detached via Ctrl+B (still running, turn released).
    pub backgrounded: bool,
}

/// Build the status-line rows: a generation/tool spinner followed by one row
/// per queued message.
///
/// When the spinner fits in `width` it stays on one row. When it doesn't (a
/// long task headline), it splits into exactly two rows — the status text on
/// row 1, the `(esc to interrupt …)` metadata on row 2 (indented under the
/// status text) — and each row is *truncated* to `width` so nothing ever
/// bleeds off the right edge. The fixed 1-or-2-row shape keeps the reserved
/// height stable as the timer/token counter tick (no per-frame reflow).
#[expect(clippy::too_many_arguments)]
#[expect(
    clippy::too_many_lines,
    reason = "predates the lint; see .github/baselines/expect_budget.txt"
)]
#[must_use]
pub fn build_status_lines(
    status: GenerationStatus,
    elapsed_secs: u64,
    tokens_received: usize,
    tokens_estimated: bool,
    status_override: Option<&str>,
    agents: &[AgentPanelRow],
    bg_available: bool,
    task_headline: Option<&str>,
    queued_messages: &VecDeque<QueuedMessage>,
    exit_armed: bool,
    theme: &Theme,
    width: u16,
) -> Vec<Line<'static>> {
    // Idle renders nothing — except when detached background agents are still
    // running, whose rows must stay visible between turns.
    if (status == GenerationStatus::Idle && agents.is_empty()) || width < 10 {
        return Vec::new();
    }
    let width = width as usize;

    // The headline, by precedence:
    //   1. An in_progress checklist task's `active_form` (Claude Code parity —
    //      the spinner row reads as the checklist header).
    //   2. An override replacing the whole text ("Running 3 agents") when the
    //      agent panel carries the detail.
    //   3. The bare generation phase. Never the tool name or its arguments —
    //      per-tool detail belongs to the transcript's action rows, not here.
    let status_text = match (task_headline, status_override) {
        (Some(head), _) => head.to_string(),
        (None, Some(text)) => text.to_string(),
        (None, None) => status.display_text().to_string(),
    };

    let info_style = Style::new().fg(theme.colors.info.to_color());
    let meta_style = Style::new()
        .fg(theme.colors.text_secondary.to_color())
        .dim();

    // Arrow indicates message direction; the parenthetical names the flow. Only
    // the initial prompt upload is "upstream"; once the model is thinking or
    // streaming we're receiving generated tokens, so the counter flows downstream.
    let (arrow, flow_direction) = match status {
        GenerationStatus::Sending => ("↑ ", "upstream"),
        GenerationStatus::Thinking | GenerationStatus::Streaming => ("↓ ", "downstream"),
        GenerationStatus::RunningTools => ("• ", "tools"),
        GenerationStatus::Compacting => ("• ", "compaction"),
        GenerationStatus::Cancelling => ("• ", "cleanup"),
        GenerationStatus::Idle => ("", ""),
    };

    // While detachable tools run (shell commands, agents), advertise Ctrl+B.
    // Hidden when nothing running can actually background — the hint used to
    // render unconditionally and lie during read/edit-only turns.
    let bg_hint = if status == GenerationStatus::RunningTools && bg_available {
        " • ctrl+b to background"
    } else {
        ""
    };

    // A first Ctrl+C armed the exit confirmation: lead the meta with the
    // second-press hint while the window is open.
    let exit_hint = if exit_armed {
        "ctrl+c again to exit • "
    } else {
        ""
    };

    let head = format!("{status_text}... ");
    let meta = format!(
        "({exit_hint}esc to interrupt{bg_hint} • {}s • {} {}{} tokens)",
        elapsed_secs,
        // The counter is generated (received) tokens, so it reads downstream even
        // while tools run — the run total just holds steady between model calls.
        match flow_direction {
            "downstream" | "tools" => "↓",
            "compaction" => "compact",
            "cleanup" => "cleanup",
            _ => "↑",
        },
        if tokens_estimated { "~" } else { "" },
        tokens_received
    );

    let arrow_w = arrow.width();
    let single_w = arrow_w + head.width() + meta.width();

    let mut lines: Vec<Line<'static>> = Vec::new();
    if status == GenerationStatus::Idle {
        // Idle-with-background-agents: rows only, no spinner head.
    } else if single_w <= width {
        // Fits on one row — keep it compact (the common case).
        lines.push(Line::from(vec![
            Span::styled(arrow, info_style),
            Span::styled(head, info_style),
            Span::styled(meta, meta_style),
        ]));
    } else {
        // Too wide: status text on row 1, metadata on row 2 (indented under the
        // text). Truncate each so a long command or path can't bleed off-screen.
        let head_budget = width.saturating_sub(arrow_w);
        lines.push(Line::from(vec![
            Span::styled(arrow, info_style),
            Span::styled(truncate_to_cells(head.trim_end(), head_budget), info_style),
        ]));
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                truncate_to_cells(&meta, width.saturating_sub(2)),
                meta_style,
            ),
        ]));
    }

    // Live agent rows: one stable row per running/backgrounded subagent.
    // Description leads in the accent color; activity/elapsed/tokens trail in
    // the muted meta style. Counters tick but the row COUNT stays stable, so
    // the transcript never reflows mid-run.
    for row in agents.iter().take(MAX_AGENT_ROWS) {
        let marker = if row.backgrounded { "◦ bg " } else { "◦ " };
        let desc = format!("  {marker}{}", row.description);
        let mut bits: Vec<String> = Vec::new();
        if !row.activity.is_empty() {
            bits.push(row.activity.clone());
        }
        bits.push(format!("{}s", row.elapsed_secs));
        if row.tokens > 0 {
            bits.push(format!(
                "↓ ~{} tokens",
                mermaid_domain::compaction::format_compact_count(row.tokens)
            ));
        }
        let desc_budget = width.min(desc.width());
        let meta_budget = width.saturating_sub(desc_budget + 2);
        let mut spans = vec![Span::styled(
            truncate_to_cells(&desc, width),
            Style::new().fg(theme.colors.info.to_color()),
        )];
        if meta_budget > 3 {
            spans.push(Span::styled(
                format!("  {}", truncate_to_cells(&bits.join(" · "), meta_budget)),
                meta_style,
            ));
        }
        lines.push(Line::from(spans));
    }
    if agents.len() > MAX_AGENT_ROWS {
        lines.push(Line::from(vec![Span::styled(
            format!("  … +{} more", agents.len() - MAX_AGENT_ROWS),
            meta_style,
        )]));
    }

    // Queued messages: one truncated, highlighted row each (bounded count).
    let body_budget = width.saturating_sub(2); // "> " prefix
    for queued in queued_messages.iter().take(MAX_QUEUED_ROWS) {
        lines.push(Line::from(vec![Span::styled(
            format!("> {}", truncate_to_cells(&queued.text, body_budget)),
            Style::new()
                .fg(theme.colors.text_primary.to_color())
                .bg(theme.colors.queued_bg.to_color()),
        )]));
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::theme::Theme;

    fn row_width(line: &Line<'_>) -> usize {
        line.spans.iter().map(|s| s.content.as_ref().width()).sum()
    }

    #[test]
    fn long_task_headline_splits_and_fits_width() {
        let theme = Theme::dark();
        let queued = VecDeque::new();
        let lines = build_status_lines(
            GenerationStatus::RunningTools,
            3,
            0,
            false,
            None,
            &[],
            true,
            Some("Rewiring the provider factory so runtime toggles ride on ChatRequest end to end"),
            &queued,
            false,
            &theme,
            80,
        );
        // Splits into status row + metadata row, neither exceeding the width.
        assert_eq!(lines.len(), 2, "a too-wide status splits onto two rows");
        for l in &lines {
            assert!(
                row_width(l) <= 80,
                "row exceeds width: {} > 80",
                row_width(l)
            );
        }
    }

    #[test]
    fn running_tools_headline_is_the_bare_phase_word() {
        // Regression: the in-flight tool (name + command/path) used to be
        // folded into the spinner headline ("Running tools: Bash pwd; …").
        // Per-tool detail belongs to the transcript; the status line must
        // never carry it.
        let theme = Theme::dark();
        let queued = VecDeque::new();
        let lines = build_status_lines(
            GenerationStatus::RunningTools,
            11,
            169,
            false,
            None,
            &[],
            true,
            None,
            &queued,
            false,
            &theme,
            120,
        );
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(
            text.contains("Running tools..."),
            "bare phase word expected: {text}"
        );
        assert!(
            !text.contains(':'),
            "no tool detail may follow the phase word: {text}"
        );
    }

    #[test]
    fn thinking_shows_downstream_arrow_and_live_token_count() {
        // Regression: the live counter sat at 0 through the (often long) thinking
        // phase. It must climb and read as received (downstream) tokens.
        let theme = Theme::dark();
        let queued = VecDeque::new();
        let lines = build_status_lines(
            GenerationStatus::Thinking,
            7,
            1_234,
            true,
            None,
            &[],
            true,
            None,
            &queued,
            false,
            &theme,
            120,
        );
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(text.contains("1234"), "must show the live count: {text}");
        assert!(
            text.contains('↓'),
            "thinking receives tokens (downstream): {text}"
        );
        assert!(!text.contains('↑'), "thinking is not upstream: {text}");
    }

    #[test]
    fn unbreakable_long_headline_is_truncated_not_overflowed() {
        // Regression (review finding): a long whitespace-free headline used to
        // be pushed whole onto a row and still bleed off the right edge.
        let theme = Theme::dark();
        let queued = VecDeque::new();
        let lines = build_status_lines(
            GenerationStatus::RunningTools,
            1,
            0,
            false,
            None,
            &[],
            true,
            Some("Editing D:/Code/AI/some/very/deeply/nested/directory/structure/longfilename.rs"),
            &queued,
            false,
            &theme,
            40,
        );
        for l in &lines {
            assert!(
                row_width(l) <= 40,
                "no row may exceed width even for an unbreakable path: {} > 40",
                row_width(l)
            );
        }
    }

    #[test]
    fn short_status_stays_one_row() {
        let theme = Theme::dark();
        let queued = VecDeque::new();
        let lines = build_status_lines(
            GenerationStatus::Sending,
            0,
            0,
            false,
            None,
            &[],
            true,
            None,
            &queued,
            false,
            &theme,
            120,
        );
        assert_eq!(lines.len(), 1, "a short status stays on one row");
    }

    #[test]
    fn height_is_stable_as_metadata_ticks() {
        // The 1-or-2-row shape must not flip as elapsed/token counters grow, so
        // the chat transcript doesn't reflow every second.
        let theme = Theme::dark();
        let queued = VecDeque::new();
        let headline = Some("Running the full local gate across every workspace crate and target");
        let n0 = build_status_lines(
            GenerationStatus::RunningTools,
            9,
            99,
            false,
            None,
            &[],
            true,
            headline,
            &queued,
            false,
            &theme,
            100,
        )
        .len();
        for (elapsed, tokens) in [(10, 100), (999, 100000), (3600, 999999)] {
            let n = build_status_lines(
                GenerationStatus::RunningTools,
                elapsed,
                tokens,
                false,
                None,
                &[],
                true,
                headline,
                &queued,
                false,
                &theme,
                100,
            )
            .len();
            assert_eq!(n, n0, "row count must not change as counters tick");
        }
    }

    #[test]
    fn armed_exit_shows_second_press_hint() {
        let theme = Theme::dark();
        let queued = VecDeque::new();
        let lines = build_status_lines(
            GenerationStatus::Streaming,
            2,
            10,
            true,
            None,
            &[],
            true,
            None,
            &queued,
            true,
            &theme,
            120,
        );
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(
            text.contains("ctrl+c again to exit"),
            "armed exit must surface the second-press hint: {text}"
        );
    }

    #[test]
    fn idle_status_is_empty() {
        let theme = Theme::dark();
        let queued = VecDeque::new();
        let lines = build_status_lines(
            GenerationStatus::Idle,
            0,
            0,
            false,
            None,
            &[],
            true,
            None,
            &queued,
            false,
            &theme,
            80,
        );
        assert!(lines.is_empty(), "idle has no status row");
    }
}
