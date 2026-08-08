//! The `/plan config` settings picker: a bottom-pane modal listing every
//! plan-mode setting as a cyclable row. Pure ASCII, muted-gray meta text.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Widget},
};

use crate::render::theme::Theme;
use mermaid_domain::PlanConfig;

/// Row count (kept in sync with `plan_config_rows` and the reducer's key
/// handler). Rows: preset, builds, web, memory, tasks, model, reasoning,
/// `auto_approve`, `post_approve`.
pub const PLAN_CONFIG_ROWS: usize = 9;

/// Pane height: rows + border(2) + hint line.
pub const PLAN_CONFIG_HEIGHT: u16 = PLAN_CONFIG_ROWS as u16 + 3;

/// The `(label, value)` pairs the picker shows, derived from the live
/// config. Shared with the reducer tests so row indices can't drift.
pub fn plan_config_rows(plan: &PlanConfig, session_model: &str) -> Vec<(String, String)> {
    let perms = &plan.permissions;
    vec![
        (
            "permissions".to_string(),
            perms.preset_name().unwrap_or("custom").to_string(),
        ),
        (
            "  builds/tests".to_string(),
            perms.builds.as_str().to_string(),
        ),
        ("  web".to_string(), perms.web.as_str().to_string()),
        (
            "  memory writes".to_string(),
            perms.memory.as_str().to_string(),
        ),
        ("  task tools".to_string(), perms.tasks.as_str().to_string()),
        (
            "plan model".to_string(),
            plan.model.clone().unwrap_or_else(|| {
                format!("unset (plans with the session model, now {session_model})")
            }),
        ),
        (
            "plan reasoning".to_string(),
            plan.reasoning
                .map(|r| r.as_str().to_string())
                .unwrap_or_else(|| "unset".to_string()),
        ),
        (
            "auto-approve plans".to_string(),
            if plan.auto_approve { "on" } else { "off" }.to_string(),
        ),
        (
            "after approval".to_string(),
            match plan.post_approve {
                None => "ask each time".to_string(),
                Some(mermaid_domain::PlanPostApprove::Start) => "always start".to_string(),
                Some(mermaid_domain::PlanPostApprove::Wait) => "always wait".to_string(),
            },
        ),
    ]
}

pub struct PlanConfigWidget<'a> {
    pub theme: &'a Theme,
    pub plan: &'a PlanConfig,
    pub session_model: &'a str,
    pub cursor: usize,
}

impl<'a> Widget for PlanConfigWidget<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let rows = plan_config_rows(self.plan, self.session_model);
        let selected_style = Style::new().fg(self.theme.colors.info.to_color()).bold();
        let label_style = Style::new().fg(self.theme.colors.text_primary.to_color());
        let value_style = Style::new().fg(self.theme.colors.text_secondary.to_color());
        let hint_style = Style::new().fg(self.theme.colors.text_disabled.to_color());

        let mut lines: Vec<Line> = rows
            .iter()
            .enumerate()
            .map(|(i, (label, value))| {
                let marker = if i == self.cursor { "> " } else { "  " };
                let ls = if i == self.cursor {
                    selected_style
                } else {
                    label_style
                };
                Line::from(vec![
                    Span::styled(format!("{marker}{label:<18}"), ls),
                    Span::styled(value.clone(), value_style),
                ])
            })
            .collect();
        lines.push(Line::from(Span::styled(
            "Enter/Left/Right change - Up/Down navigate - Esc close",
            hint_style,
        )));

        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Plan mode settings "),
            )
            .render(area, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_count_matches_the_reducer_contract() {
        // The reducer's key handler hardcodes PLAN_CONFIG_ROW_COUNT = 9;
        // this pins the widget to the same shape so indices can't drift.
        let rows = plan_config_rows(&PlanConfig::default(), "ollama/test");
        assert_eq!(rows.len(), PLAN_CONFIG_ROWS);
        assert_eq!(rows.len(), 9);
        assert_eq!(rows[0].1, "default");
        assert_eq!(rows[4].1, "deny");
        assert!(rows[5].1.starts_with("unset"));
    }
}
