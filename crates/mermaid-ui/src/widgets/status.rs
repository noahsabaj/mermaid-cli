use unicode_width::UnicodeWidthStr;

use crate::node::{Line, Span, UiNode};
use crate::theme::{StyleToken, Theme, ThemeToken};
use mermaid_domain::{ContextUsageSnapshot, format_compact_count};
use mermaid_model::models::{ReasoningLevel, TokenUsageSource};
use mermaid_model::safety::SafetyMode;

#[derive(Debug, Clone)]
pub struct StatusProps<'a> {
    pub theme: &'a Theme,
    pub working_dir: &'a str,
    pub hostname: &'a str,
    pub username: &'a str,
    pub version: &'a str,
    pub context_usage: Option<&'a ContextUsageSnapshot>,
    pub model_name: &'a str,
    pub reasoning_level: ReasoningLevel,
    pub requested_level: Option<ReasoningLevel>,
    pub safety_mode: SafetyMode,
    pub width: usize,
}

#[must_use]
pub fn build_status_view(props: StatusProps<'_>) -> UiNode {
    let directory_text = format!(
        "{}@{}:{}",
        props.username, props.hostname, props.working_dir
    );
    let token_text = format_token_status(props.context_usage);

    let available_width = props.width;
    let directory_width = directory_text.width();
    let token_width = token_text.width();
    let padding_width = if available_width > directory_width + token_width + 1 {
        available_width - directory_width - token_width
    } else {
        1
    };

    let line1_spans = vec![
        Span::styled(
            format!("{}@{}", props.username, props.hostname),
            StyleToken::new().fg(ThemeToken::Success).bold(),
        ),
        Span::styled(":", StyleToken::new().fg(ThemeToken::TextPrimary)),
        Span::styled(props.working_dir, StyleToken::new().fg(ThemeToken::Info)),
        Span::raw(" ".repeat(padding_width)),
        Span::styled(token_text, StyleToken::new().fg(ThemeToken::TextDisabled)),
    ];

    let reasoning_text = match props.requested_level {
        Some(requested) => format!(
            "reasoning: {} ({} requested)",
            props.reasoning_level.as_str(),
            requested.as_str()
        ),
        None => format!("reasoning: {}", props.reasoning_level.as_str()),
    };
    let safety_segment = format!("safety: {}", props.safety_mode.as_str());
    let left_text = status_line2_left(props.version, &safety_segment, &reasoning_text);
    let model_display = props.model_name;

    let left_content_width = left_text.width();
    let right_content_width = model_display.width();
    let padding_width_line2 = if available_width > left_content_width + right_content_width {
        available_width - left_content_width - right_content_width
    } else {
        1
    };

    let line2_spans = vec![
        Span::styled(left_text, StyleToken::new().fg(ThemeToken::TextDisabled)),
        Span::raw(" ".repeat(padding_width_line2)),
        Span::styled(
            model_display,
            StyleToken::new().fg(ThemeToken::TextDisabled),
        ),
    ];

    UiNode::text(vec![Line::from(line1_spans), Line::from(line2_spans)])
}

#[must_use]
pub fn format_token_status(context_usage: Option<&ContextUsageSnapshot>) -> String {
    match context_usage {
        Some(snapshot) => format_context_snapshot(snapshot),
        None => "context: n/a".to_string(),
    }
}

fn format_context_snapshot(snapshot: &ContextUsageSnapshot) -> String {
    let used = format_compact_count(snapshot.used_tokens);
    let source = match snapshot.source {
        TokenUsageSource::Provider => "",
        TokenUsageSource::Estimate => "~",
    };
    match (snapshot.max_tokens, snapshot.used_percent) {
        (Some(max), Some(percent)) => format!(
            "context: {}{} / {} ({}%)",
            source,
            used,
            format_compact_count(max),
            percent
        ),
        _ => format!("context: {source}{used} / unknown"),
    }
}

fn status_line2_left(version: &str, safety_segment: &str, reasoning_text: &str) -> String {
    format!("mermaid v{version} · {safety_segment} · {reasoning_text}")
}
