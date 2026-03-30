//! Tool call execution for the agent loop
//!
//! This module handles executing Ollama native tool calls and building
//! UI displays for the results.

use crate::agents::{
    self, ActionDetails, ActionDisplay, ActionResult as AgentActionResult, AgentAction,
    SubagentResult, execute_action,
};
use crate::models::{MessageRole, ToolCall};
use crate::runtime::agent_loop::ToolExecutionResult;
use crate::tui::App;

/// Execute tool calls and return results for the agent loop
///
/// This is the main function for the agentic flow. It:
/// 1. Takes tool_calls from the model response
/// 2. Converts each to an AgentAction
/// 3. Executes the action directly (no confirmation)
/// 4. Returns ToolExecutionResult for each
pub async fn execute_tool_calls_for_agent_loop(
    app: &mut App,
    tool_calls: &[ToolCall],
) -> Vec<ToolExecutionResult> {
    let mut results = Vec::new();

    for tool_call in tool_calls {
        let tool_call_id = tool_call.id.clone().unwrap_or_else(|| {
            use std::time::{SystemTime, UNIX_EPOCH};
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            format!("call_{:x}", timestamp)
        });
        let tool_name = tool_call.function.name.clone();

        // Convert tool call to AgentAction
        let action = match tool_call.to_agent_action() {
            Ok(action) => action,
            Err(e) => {
                results.push(ToolExecutionResult {
                    tool_call_id,
                    tool_name,
                    action: AgentAction::ParseError {
                        message: format!("Error: {}", e),
                    },
                    success: false,
                    output: format!("Error: {}", e),
                });
                continue;
            },
        };

        // Execute the action directly
        let action_clone = action.clone();
        match execute_action(&action).await {
            agents::ActionResult::Success { output } => {
                // Build ActionDisplay for UI rendering
                let action_display = build_action_display(&action_clone, &output);

                // Attach ActionDisplay to the most recent assistant message
                if let Some(last_msg) = app
                    .session_state
                    .messages
                    .iter_mut()
                    .rev()
                    .find(|m| matches!(m.role, MessageRole::Assistant))
                {
                    last_msg.actions.push(action_display);
                }

                results.push(ToolExecutionResult {
                    tool_call_id,
                    tool_name,
                    action: action_clone,
                    success: true,
                    output,
                });
            },
            agents::ActionResult::Error { error } => {
                let action_display = build_error_display(&action_clone, &error);
                if let Some(last_msg) = app
                    .session_state
                    .messages
                    .iter_mut()
                    .rev()
                    .find(|m| matches!(m.role, MessageRole::Assistant))
                {
                    last_msg.actions.push(action_display);
                }

                results.push(ToolExecutionResult {
                    tool_call_id,
                    tool_name,
                    action: action_clone,
                    success: false,
                    output: format!("Error: {}", error),
                });
            },
        }
    }

    results
}

/// Build an ActionDisplay from an action and its output
fn build_action_display(action: &AgentAction, output: &str) -> ActionDisplay {
    build_action_display_with_timing(action, output, None)
}

/// Build an error ActionDisplay - uses the action for target info but wraps as Error
fn build_error_display(action: &AgentAction, error: &str) -> ActionDisplay {
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

fn build_action_display_with_timing(
    action: &AgentAction,
    output: &str,
    duration_seconds: Option<f64>,
) -> ActionDisplay {
    let (action_type, target) = action.display_info();
    let result = AgentActionResult::Success {
        output: output.to_string(),
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
        AgentAction::ParseError { .. } => ActionDetails::Simple,
    };

    ActionDisplay {
        action_type: action_type.to_string(),
        target,
        result,
        details,
        duration_seconds,
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

/// Format duration in seconds as human-readable string.
fn format_duration_secs(total_secs: f64) -> String {
    let secs = total_secs as u64;
    if secs < 60 {
        return format!("{:.1}s", total_secs);
    }
    let mins = secs / 60;
    let remainder = secs % 60;
    if mins < 60 {
        format!("{}m {}s", mins, remainder)
    } else {
        let hours = mins / 60;
        let mins = mins % 60;
        format!("{}h {}m {}s", hours, mins, remainder)
    }
}

/// Build an ActionDisplay for a completed subagent result.
pub fn build_agent_action_display(result: &SubagentResult) -> ActionDisplay {
    let token_display = crate::utils::format_tokens(result.tokens);
    let summary = format!(
        "Completed \u{00b7} {} tool uses \u{00b7} {} \u{00b7} {}",
        result.tool_uses,
        token_display,
        format_duration_secs(result.duration_secs),
    );

    ActionDisplay {
        action_type: "Agent".to_string(),
        target: result.description.clone(),
        result: if result.success {
            AgentActionResult::Success {
                output: result.response.clone(),
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
