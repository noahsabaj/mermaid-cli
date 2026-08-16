use std::collections::VecDeque;
use unicode_width::UnicodeWidthStr;

use crate::markdown::parse_markdown_inline;
use crate::node::{Line, Span};
use crate::theme::{StyleToken, Theme, ThemeToken};
use crate::wrap::truncate_to_cells;
use mermaid_domain::QueuedMessage;

const MAX_QUEUED_ROWS: usize = 5;
const MAX_AGENT_ROWS: usize = 6;

#[derive(Debug, Clone)]
pub struct AgentPanelRow {
    pub description: String,
    pub activity: String,
    pub elapsed_secs: u64,
    pub tokens: u64,
    pub backgrounded: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenerationStatus {
    Sending,
    Thinking,
    Streaming,
    RunningTools,
    Compacting,
    Cancelling,
    Idle,
}

impl GenerationStatus {
    #[must_use]
    pub const fn display_text(self) -> &'static str {
        match self {
            Self::Sending => "Sending prompt",
            Self::Thinking => "Thinking",
            Self::Streaming => "Streaming response",
            Self::RunningTools => "Running tools",
            Self::Compacting => "Compacting context",
            Self::Cancelling => "Cancelling turn",
            Self::Idle => "",
        }
    }
}

fn truncate_line_to_cells(line: Line, budget: usize) -> Line {
    let total_w: usize = line.spans.iter().map(Span::width).sum();
    if total_w <= budget {
        return line;
    }
    let budget = budget.saturating_sub(1);
    let mut current_w = 0usize;
    let mut out_spans: Vec<Span> = Vec::new();
    let mut last_style = StyleToken::new();

    for span in line.spans {
        last_style = span.style;
        let mut buf = String::new();
        for ch in span.content.as_ref().chars() {
            let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
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

#[derive(Debug, Clone)]
pub struct StatusLineProps<'a> {
    pub status: GenerationStatus,
    pub elapsed_secs: u64,
    pub tokens_received: usize,
    pub tokens_estimated: bool,
    pub status_override: Option<&'a str>,
    pub agents: &'a [AgentPanelRow],
    pub bg_available: bool,
    pub task_headline: Option<&'a str>,
    pub queued_messages: &'a VecDeque<QueuedMessage>,
    pub exit_armed: bool,
    pub theme: &'a Theme,
    pub width: u16,
}

fn format_meta_string(props: &StatusLineProps<'_>, flow: &str) -> String {
    let bg_hint = if props.status == GenerationStatus::RunningTools && props.bg_available {
        " • ctrl+b to background"
    } else {
        ""
    };
    let exit_hint = if props.exit_armed {
        "ctrl+c again to exit • "
    } else {
        ""
    };
    let flow_sym = match flow {
        "downstream" | "tools" => "↓",
        "compaction" => "compact",
        "cleanup" => "cleanup",
        _ => "↑",
    };
    let est = if props.tokens_estimated { "~" } else { "" };
    format!(
        "({exit_hint}esc to interrupt{bg_hint} • {}s • {flow_sym} {est}{} tokens)",
        props.elapsed_secs, props.tokens_received
    )
}

fn build_headline_lines(props: &StatusLineProps<'_>, lines: &mut Vec<Line>, width: usize) {
    let status_text = match (props.task_headline, props.status_override) {
        (Some(head), _) => head.to_string(),
        (None, Some(text)) => text.to_string(),
        (None, None) => props.status.display_text().to_string(),
    };

    let info_style = StyleToken::new().fg(ThemeToken::Info);
    let meta_style = StyleToken::new().fg(ThemeToken::TextSecondary).dim();

    let (arrow, flow) = match props.status {
        GenerationStatus::Sending => ("↑ ", "upstream"),
        GenerationStatus::Thinking | GenerationStatus::Streaming => ("↓ ", "downstream"),
        GenerationStatus::RunningTools => ("• ", "tools"),
        GenerationStatus::Compacting => ("• ", "compaction"),
        GenerationStatus::Cancelling => ("• ", "cleanup"),
        GenerationStatus::Idle => ("", ""),
    };

    let head_text = format!("{status_text}...");
    let parsed_head = parse_markdown_inline(&head_text, props.theme, info_style);
    let head_w: usize = parsed_head.spans.iter().map(Span::width).sum();
    let meta = format_meta_string(props, flow);

    let arrow_w = UnicodeWidthStr::width(arrow);
    let single_w = arrow_w + head_w + 1 + meta.width();

    if props.status == GenerationStatus::Idle {
        // Idle
    } else if single_w <= width {
        let mut row_spans = vec![Span::styled(arrow, info_style)];
        row_spans.extend(parsed_head.spans);
        row_spans.push(Span::styled(" ", info_style));
        row_spans.push(Span::styled(meta, meta_style));
        lines.push(Line::from(row_spans));
    } else {
        let head_budget = width.saturating_sub(arrow_w);
        let truncated_head = truncate_line_to_cells(parsed_head, head_budget);
        let mut head_row_spans = vec![Span::styled(arrow, info_style)];
        head_row_spans.extend(truncated_head.spans);
        lines.push(Line::from(head_row_spans));
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                truncate_to_cells(&meta, width.saturating_sub(2)),
                meta_style,
            ),
        ]));
    }
}

fn build_agent_and_queued_lines(props: &StatusLineProps<'_>, lines: &mut Vec<Line>, width: usize) {
    let meta_style = StyleToken::new().fg(ThemeToken::TextSecondary).dim();
    for row in props.agents.iter().take(MAX_AGENT_ROWS) {
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
                mermaid_domain::compaction::format_compact_count(row.tokens as usize)
            ));
        }
        let desc_budget = width.min(desc.width());
        let meta_budget = width.saturating_sub(desc_budget + 2);
        let mut spans = vec![Span::styled(
            truncate_to_cells(&desc, width),
            StyleToken::new().fg(ThemeToken::Info),
        )];
        if meta_budget > 3 {
            spans.push(Span::styled(
                format!("  {}", truncate_to_cells(&bits.join(" · "), meta_budget)),
                meta_style,
            ));
        }
        lines.push(Line::from(spans));
    }
    if props.agents.len() > MAX_AGENT_ROWS {
        lines.push(Line::from(vec![Span::styled(
            format!("  … +{} more", props.agents.len() - MAX_AGENT_ROWS),
            meta_style,
        )]));
    }
    let body_budget = width.saturating_sub(2);
    for queued in props.queued_messages.iter().take(MAX_QUEUED_ROWS) {
        lines.push(Line::from(vec![Span::styled(
            format!("> {}", truncate_to_cells(&queued.text, body_budget)),
            StyleToken::new()
                .fg(ThemeToken::TextPrimary)
                .bg(ThemeToken::QueuedBg),
        )]));
    }
}

#[must_use]
pub fn build_status_lines(props: StatusLineProps<'_>) -> Vec<Line> {
    if (props.status == GenerationStatus::Idle && props.agents.is_empty()) || props.width < 10 {
        return Vec::new();
    }
    let width = props.width as usize;
    let mut lines: Vec<Line> = Vec::new();
    build_headline_lines(&props, &mut lines, width);
    build_agent_and_queued_lines(&props, &mut lines, width);
    lines
}
