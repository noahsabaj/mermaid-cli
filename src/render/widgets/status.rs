use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};
use unicode_width::UnicodeWidthStr;

use super::truncate_to_cells;
use crate::render::theme::Theme;
use mermaid_domain::ContextUsageSnapshot;
use mermaid_model::models::ReasoningLevel;
use mermaid_model::safety::SafetyMode;

/// The one-line footer under the composer: the session's mode on the left,
/// the model and the context gauge on the right, everything in the theme's
/// meta colour. What it deliberately does not show: `user@host` (the shell
/// prompt and the window title already do), the working directory (the
/// session header names it once), and the version (`mermaid --version`,
/// `/doctor`). A footer is furniture; it should carry what changes.
pub struct StatusWidget<'a> {
    pub theme: &'a Theme,
    pub context_usage: Option<&'a ContextUsageSnapshot>,
    pub model_name: &'a str,
    /// Effective reasoning depth — what the API actually saw after
    /// `nearest_effort` snapping against the model's capabilities.
    pub reasoning_level: ReasoningLevel,
    /// User-requested level when it differs from `reasoning_level` (the snap
    /// case). `Some(requested)` shows `reasoning: high (max requested)`.
    pub requested_level: Option<ReasoningLevel>,
    /// Live session safety mode. Never the spinner/status widget (#245
    /// invariant) — this is the persistent mode line.
    pub safety_mode: SafetyMode,
    /// `goal 4m` while a `/goal` is set; see [`goal_segment`].
    pub goal: Option<String>,
}

impl<'a> Widget for StatusWidget<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let width = area.width as usize;
        let mut left = footer_left(self.safety_mode, self.reasoning_level, self.requested_level);
        if let Some(goal) = &self.goal {
            left.push_str(" · ");
            left.push_str(goal);
        }
        let right = footer_right(
            self.model_name,
            context_segment(self.context_usage).as_deref(),
            width.saturating_sub(left.width() + 1),
        );
        let gap = width.saturating_sub(left.width() + right.width()).max(1);
        let meta = Style::new().fg(self.theme.colors.text_meta.to_color());
        let line = Line::from(vec![
            Span::styled(left, meta),
            Span::raw(" ".repeat(gap)),
            Span::styled(right, meta),
        ]);
        Paragraph::new(vec![line]).render(area, buf);
    }
}

/// `safety: <mode> · reasoning: <level>[ (<requested> requested)]`.
pub(crate) fn footer_left(
    safety_mode: SafetyMode,
    reasoning_level: ReasoningLevel,
    requested_level: Option<ReasoningLevel>,
) -> String {
    let reasoning = match requested_level {
        Some(requested) => format!(
            "reasoning: {} ({} requested)",
            reasoning_level.as_str(),
            requested.as_str()
        ),
        None => format!("reasoning: {}", reasoning_level.as_str()),
    };
    format!("safety: {} · {reasoning}", safety_mode.as_str())
}

/// `goal` while a `/goal` is set, with how long it has run once it has
/// started (a goal restored by `--resume` has not, until the next message).
pub(crate) fn goal_segment(
    active: bool,
    started: Option<std::time::SystemTime>,
    now: std::time::SystemTime,
) -> Option<String> {
    if !active {
        return None;
    }
    // Whole minutes: a footer that ticks every second is noise.
    Some(match started.and_then(|t| now.duration_since(t).ok()) {
        Some(elapsed) => {
            let minutes = elapsed.as_secs() / 60;
            if minutes < 60 {
                format!("goal {minutes}m")
            } else {
                format!("goal {}h {}m", minutes / 60, minutes % 60)
            }
        },
        None => "goal".to_string(),
    })
}

/// The context gauge, or nothing until the provider has reported usage:
/// `context: n/a` at every session start was a null value dressed as
/// information.
pub(crate) fn context_segment(context_usage: Option<&ContextUsageSnapshot>) -> Option<String> {
    let snapshot = context_usage?;
    let percent = snapshot.used_percent?;
    Some(format!("context {percent}%"))
}

/// `<model>[ · context N%]`, fitted into `budget` cells: when the full id does
/// not fit, the provider prefix goes first (the header names it, and the
/// vendor namespace is the part that distinguishes models), then the text is
/// truncated.
pub(crate) fn footer_right(model: &str, context: Option<&str>, budget: usize) -> String {
    let assemble = |model: &str| match context {
        Some(ctx) => format!("{model} · {ctx}"),
        None => model.to_string(),
    };
    let full = assemble(model);
    if full.width() <= budget {
        return full;
    }
    let short = model.split_once('/').map_or(model, |(_, rest)| rest);
    let shorter = assemble(short);
    if shorter.width() <= budget {
        return shorter;
    }
    truncate_to_cells(&shorter, budget)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn footer_left_shows_safety_and_reasoning() {
        let s = footer_left(SafetyMode::Ask, ReasoningLevel::High, None);
        assert_eq!(s, "safety: ask · reasoning: high");
        let s = footer_left(
            SafetyMode::Ask,
            ReasoningLevel::High,
            Some(ReasoningLevel::Max),
        );
        assert_eq!(s, "safety: ask · reasoning: high (max requested)");
    }

    #[test]
    fn goal_segment_shows_only_while_a_goal_is_set() {
        let now = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000);
        let started = now - std::time::Duration::from_secs(240);
        assert_eq!(goal_segment(false, Some(started), now), None);
        assert_eq!(goal_segment(true, None, now).as_deref(), Some("goal"));
        assert_eq!(
            goal_segment(true, Some(started), now).as_deref(),
            Some("goal 4m")
        );
    }

    #[test]
    fn context_segment_is_absent_until_usage_is_known() {
        assert_eq!(context_segment(None), None);
        let estimate = ContextUsageSnapshot::from_estimate(
            mermaid_domain::PromptTokenBreakdown {
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
            context_segment(Some(&estimate)),
            None,
            "no window, no percent"
        );
        let known = ContextUsageSnapshot::from_usage(
            &mermaid_model::models::TokenUsage::provider(12_000, 456),
            Some(128_000),
        );
        assert_eq!(context_segment(Some(&known)).as_deref(), Some("context 9%"));
    }

    #[test]
    fn footer_right_drops_the_provider_before_it_truncates() {
        let full = footer_right("anthropic/claude-sonnet-4-5", Some("context 100%"), 80);
        assert_eq!(full, "anthropic/claude-sonnet-4-5 · context 100%");
        let squeezed = footer_right("anthropic/claude-sonnet-4-5", Some("context 100%"), 34);
        assert_eq!(squeezed, "claude-sonnet-4-5 · context 100%");
        let cut = footer_right("anthropic/claude-sonnet-4-5", Some("context 100%"), 12);
        assert!(cut.width() <= 12, "{cut}");
    }
}
