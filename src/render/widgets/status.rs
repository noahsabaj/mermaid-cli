use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};
use unicode_width::UnicodeWidthStr;

use crate::domain::{ContextUsageSnapshot, format_compact_count};
use crate::models::{ReasoningLevel, TokenUsageSource};
use crate::render::theme::Theme;
use crate::runtime::SafetyMode;

/// Props for StatusWidget (stateless widget)
pub struct StatusWidget<'a> {
    pub theme: &'a Theme,
    pub working_dir: &'a str,
    /// Hostname + username for the `user@host:cwd` line, resolved once at
    /// startup and threaded in (#55) rather than read from the environment on
    /// every frame.
    pub hostname: &'a str,
    pub username: &'a str,
    pub context_usage: Option<&'a ContextUsageSnapshot>,
    pub model_name: &'a str,
    /// Effective reasoning depth — what the API actually saw after
    /// `nearest_effort` snapping against the model's capabilities. Always
    /// rendered on line 2 left.
    pub reasoning_level: ReasoningLevel,
    /// User-requested level when it differs from `reasoning_level` (the
    /// snap case). `Some(requested)` shows `reasoning: high (max
    /// requested)`; `None` shows just `reasoning: high`.
    pub requested_level: Option<ReasoningLevel>,
    /// Live session safety mode. Rendered on line 2 left so the active
    /// permission level is always visible (Shift+Tab / `/safety` change it).
    pub safety_mode: SafetyMode,
}

impl<'a> Widget for StatusWidget<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // Line 1: username@hostname:/path (left) | token usage (right, fixed position).
        // Host/user are resolved once at startup and passed in (#55).
        let directory_text = format!("{}@{}:{}", self.username, self.hostname, self.working_dir);
        let token_text = format_token_status(self.context_usage);

        // Calculate padding to push tokens to right edge. Use display-cell
        // widths so CJK / emoji chars in working_dir or hostname don't
        // misalign the right-anchored token count.
        let available_width = area.width as usize;
        let directory_width = directory_text.width();
        let token_width = token_text.width();
        let padding_width = if available_width > directory_width + token_width + 1 {
            available_width - directory_width - token_width
        } else {
            1
        };

        let line1_spans = vec![
            // Directory (fixed to left)
            Span::styled(
                format!("{}@{}", self.username, self.hostname),
                Style::new().fg(self.theme.colors.success.to_color()).bold(),
            ),
            Span::styled(
                ":",
                Style::new().fg(self.theme.colors.text_primary.to_color()),
            ),
            Span::styled(
                self.working_dir,
                Style::new().fg(self.theme.colors.info.to_color()),
            ),
            // Padding
            Span::raw(" ".repeat(padding_width)),
            // Token count (fixed to right)
            Span::styled(
                token_text,
                Style::new().fg(self.theme.colors.text_disabled.to_color()),
            ),
        ];

        // Line 2: "reasoning: <level>" (or "<level> (<requested> requested)"
        // when the user's requested level got snapped to a lower one by
        // the model's capability ceiling) | model name (right).
        let reasoning_text = match self.requested_level {
            Some(requested) => format!(
                "reasoning: {} ({} requested)",
                self.reasoning_level.as_str(),
                requested.as_str()
            ),
            None => format!("reasoning: {}", self.reasoning_level.as_str()),
        };
        // Prefix the app version (the one inoffensive, always-visible place we
        // surface it) and the live safety mode (Shift+Tab / `/safety` change it
        // live) ahead of the reasoning level.
        let left_text = status_line2_left(self.safety_mode.as_str(), &reasoning_text);
        let model_display = self.model_name;

        // Calculate padding between reasoning text and model name (display-cell widths).
        let left_content_width = left_text.width();
        let right_content_width = model_display.width();
        let padding_width_line2 = if available_width > left_content_width + right_content_width {
            available_width - left_content_width - right_content_width
        } else {
            1
        };

        let line2_spans = vec![
            // "safety: <mode> · reasoning: <level>" (left, gray, always rendered)
            Span::styled(
                left_text,
                Style::new().fg(self.theme.colors.text_disabled.to_color()),
            ),
            // Padding to right-align model name
            Span::raw(" ".repeat(padding_width_line2)),
            // Model name (right, aligned with tokens above)
            Span::styled(
                model_display,
                Style::new().fg(self.theme.colors.text_disabled.to_color()),
            ),
        ];

        let line1 = Line::from(line1_spans);
        let line2 = Line::from(line2_spans);
        let status_bar = Paragraph::new(vec![line1, line2]);

        status_bar.render(area, buf);
    }
}

/// The footer shows the context gauge only: cumulative session usage is a
/// cost-accounting number (input re-sent per API call, subagents included)
/// that dwarfs and confuses the window meter — it lives in `/usage` with
/// labels instead.
pub(crate) fn format_token_status(context_usage: Option<&ContextUsageSnapshot>) -> String {
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
        _ => format!("context: {}{} / unknown", source, used),
    }
}

/// Left segment of status line 2: the app version (compile-time, so it tracks
/// the crate version automatically), then the live safety mode and reasoning
/// level. This footer is the single place the version is surfaced in the TUI.
fn status_line2_left(safety: &str, reasoning_text: &str) -> String {
    format!(
        "mermaid v{} · safety: {} · {}",
        env!("CARGO_PKG_VERSION"),
        safety,
        reasoning_text
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_line2_left_shows_version_safety_and_reasoning() {
        let s = status_line2_left("ask", "reasoning: high");
        assert!(
            s.contains(&format!("mermaid v{}", env!("CARGO_PKG_VERSION"))),
            "status line must show the app version — got {s:?}"
        );
        assert!(s.contains("safety: ask"));
        assert!(s.contains("reasoning: high"));
    }

    #[test]
    fn token_status_shows_context_gauge_only() {
        let context = ContextUsageSnapshot::from_usage(
            &crate::models::TokenUsage::provider(12_000, 456),
            Some(128_000),
        );
        assert_eq!(
            format_token_status(Some(&context)),
            "context: 12.4k / 128k (9%)"
        );
    }

    #[test]
    fn token_status_handles_missing_context() {
        assert_eq!(format_token_status(None), "context: n/a");
    }

    #[test]
    fn token_status_marks_estimates() {
        let context = ContextUsageSnapshot::from_estimate(
            crate::domain::PromptTokenBreakdown {
                system_tokens: 10,
                instructions_tokens: 0,
                message_tokens: 20,
                tool_schema_tokens: 70,
                image_count: 0,
                message_count: 1,
                tool_count: 4,
            },
            None,
        );

        assert_eq!(
            format_token_status(Some(&context)),
            "context: ~100 / unknown"
        );
    }
}
