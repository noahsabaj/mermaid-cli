//! Build UI displays for tool call results.
//!
//! The `TuiObserver` in `loop_coordinator::run_turn` calls these from
//! its `on_tool_result` hook to attach rich action displays (diffs,
//! file previews, etc.) under the most recent assistant message.

use crate::agents::{
    ActionDetails, ActionDisplay, ActionResult as AgentActionResult, AgentAction, SubagentResult,
};

/// Build an ActionDisplay from an action and its output. `duration_seconds`
/// stays `None` here — only `build_agent_action_display` records timing
/// (subagent runs), and the renderer only consults the field for Agent
/// actions.
pub fn build_action_display(action: &AgentAction, output: &str) -> ActionDisplay {
    let (action_type, target) = action.display_info();
    let result = AgentActionResult::Success {
        output: output.to_string(),
        images: None,
    };

    let details = match action {
        AgentAction::WriteFile { content, .. } => ActionDetails::FileContent {
            line_count: content.lines().count(),
            content: content.clone(),
        },
        AgentAction::EditFile {
            old_string,
            new_string,
            ..
        } => {
            let added = new_string.lines().count();
            let removed = old_string.lines().count();
            ActionDetails::Diff {
                summary: format!("Added {} lines, removed {} lines", added, removed),
                diff: output.to_string(),
            }
        },
        AgentAction::ReadFile { paths } => {
            let preview_lines = if paths.len() == 1 { 3 } else { 5 };
            ActionDetails::Preview {
                text: truncate_output(output, preview_lines),
                line_count: Some(output.lines().count()),
            }
        },
        AgentAction::ExecuteCommand { .. } => ActionDetails::Preview {
            text: truncate_output(output, 5),
            line_count: Some(output.lines().count()),
        },
        AgentAction::DeleteFile { .. } | AgentAction::CreateDirectory { .. } => {
            ActionDetails::Simple
        },
        AgentAction::WebSearch { .. } => {
            let is_error = !output.contains("[SEARCH_RESULTS]");
            let result_count = output.matches("Title:").count();
            let text = if is_error {
                truncate_output(output, 2)
            } else {
                format!("Fetched {} search results", result_count)
            };
            ActionDetails::Preview {
                text,
                line_count: Some(result_count),
            }
        },
        AgentAction::WebFetch { .. } => ActionDetails::Preview {
            text: truncate_output(output, 3),
            line_count: Some(output.lines().count()),
        },
        AgentAction::SpawnAgent { .. } => ActionDetails::Preview {
            text: truncate_output(output, 3),
            line_count: None,
        },
        AgentAction::Screenshot { .. } => ActionDetails::Preview {
            text: truncate_output(output, 2),
            line_count: None,
        },
        AgentAction::Click { .. } | AgentAction::TypeText { .. } | AgentAction::PressKey { .. } => {
            ActionDetails::Preview {
                text: truncate_output(output, 2),
                line_count: None,
            }
        },
        AgentAction::Scroll { .. } | AgentAction::MouseMove { .. } => ActionDetails::Simple,
        AgentAction::ListWindows => ActionDetails::Preview {
            text: truncate_output(output, 10),
            line_count: Some(output.lines().count()),
        },
        AgentAction::McpToolCall { .. } => ActionDetails::Preview {
            text: truncate_output(output, 5),
            line_count: Some(output.lines().count()),
        },
        AgentAction::ParseError { .. } => ActionDetails::Simple,
    };

    ActionDisplay {
        action_type: action_type.to_string(),
        target,
        result,
        details,
        duration_seconds: None,
    }
}

/// Build an error ActionDisplay - uses the action for target info but wraps as Error
pub fn build_error_display(action: &AgentAction, error: &str) -> ActionDisplay {
    let (action_type, target) = action.display_info();
    ActionDisplay {
        action_type: action_type.to_string(),
        target,
        result: AgentActionResult::Error {
            error: error.to_string(),
        },
        details: ActionDetails::Simple,
        duration_seconds: None,
    }
}

/// Truncate output to N lines with ellipsis indicator
fn truncate_output(output: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = output.lines().collect();
    if lines.len() <= max_lines {
        output.to_string()
    } else {
        let truncated = lines[..max_lines].join("\n");
        format!(
            "{}\n... ({} more lines)",
            truncated,
            lines.len() - max_lines
        )
    }
}

/// Build an ActionDisplay for a completed subagent result.
pub fn build_agent_action_display(result: &SubagentResult) -> ActionDisplay {
    let token_display = crate::utils::format_tokens(result.tokens);
    let summary = format!(
        "Completed \u{00b7} {} tool uses \u{00b7} {} \u{00b7} {}",
        result.tool_uses,
        token_display,
        crate::utils::format_duration(result.duration_secs),
    );

    ActionDisplay {
        action_type: "Agent".to_string(),
        target: result.description.clone(),
        result: if result.success {
            AgentActionResult::Success {
                output: result.response.clone(),
                images: None,
            }
        } else {
            AgentActionResult::Error {
                error: result.response.clone(),
            }
        },
        details: ActionDetails::Agent {
            summary,
            tool_uses: result.tool_uses,
        },
        duration_seconds: Some(result.duration_secs),
    }
}
