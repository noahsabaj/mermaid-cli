use crate::node::{BorderStyle, Line, Span, UiNode};
use crate::theme::{StyleToken, Theme, ThemeToken};
use mermaid_domain::PlanConfig;

pub const PLAN_CONFIG_ROWS: usize = 9;
pub const PLAN_CONFIG_HEIGHT: u16 = PLAN_CONFIG_ROWS as u16 + 3;

#[must_use]
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

#[derive(Debug, Clone)]
pub struct PlanConfigProps<'a> {
    pub theme: &'a Theme,
    pub plan: &'a PlanConfig,
    pub session_model: &'a str,
    pub cursor: usize,
}

#[must_use]
pub fn build_plan_config_view(props: PlanConfigProps<'_>) -> UiNode {
    let rows = plan_config_rows(props.plan, props.session_model);
    let selected_style = StyleToken::new().fg(ThemeToken::Info).bold();
    let label_style = StyleToken::new().fg(ThemeToken::TextPrimary);
    let value_style = StyleToken::new().fg(ThemeToken::TextSecondary);
    let hint_style = StyleToken::new().fg(ThemeToken::TextDisabled);

    let mut lines: Vec<Line> = rows
        .iter()
        .enumerate()
        .map(|(i, (label, value))| {
            let marker = if i == props.cursor { "> " } else { "  " };
            let ls = if i == props.cursor {
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
        " ↑↓ navigate · Space/Enter cycle · Esc/p dismiss",
        hint_style,
    )));

    UiNode::vertical(vec![UiNode::text(lines)], vec![])
        .with_border(BorderStyle::Plain, Some(ThemeToken::Border))
        .with_title("Plan mode settings (/plan config)".to_string())
}
