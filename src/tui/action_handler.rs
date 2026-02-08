/// Tool call execution for the agent loop
///
/// This module handles executing Ollama native tool calls and building
/// UI displays for the results.

use crate::agents::{
    self, execute_action, ActionDisplay, ActionResult as AgentActionResult, AgentAction,
};
use crate::models::{MessageRole, ToolCall};
use crate::tui::App;

/// Result of executing a tool call
/// Used for building proper Tool messages in the agent loop
#[derive(Debug, Clone)]
pub struct ToolExecutionResult {
    /// The original tool call ID (for linking result back to call)
    pub tool_call_id: String,
    /// The function name that was called
    pub tool_name: String,
    /// The result content (success output or error message)
    pub content: String,
}

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
                    content: format!("Error: {}", e),
                });
                continue;
            }
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
                    content: output,
                });
            }
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
                    content: format!("Error: {}", error),
                });
            }
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
    let (action_type, target) = match action {
        AgentAction::EditFile { path, .. } => ("Edit", path.clone()),
        AgentAction::WriteFile { path, .. } => ("Write", path.clone()),
        AgentAction::ReadFile { paths } => {
            if paths.len() == 1 { ("Read", paths[0].clone()) }
            else { ("Read", format!("{} files", paths.len())) }
        }
        AgentAction::DeleteFile { path } => ("Delete", path.clone()),
        AgentAction::CreateDirectory { path } => ("Bash", format!("mkdir -p {}", path)),
        AgentAction::ExecuteCommand { command, .. } => ("Bash", command.clone()),
        AgentAction::GitDiff { paths } => {
            let path_str = paths.first()
                .and_then(|p| p.as_ref())
                .cloned()
                .unwrap_or_else(|| ".".to_string());
            ("Bash", format!("git diff {}", path_str))
        }
        AgentAction::GitStatus => ("Bash", "git status".to_string()),
        AgentAction::GitCommit { message, .. } => ("Bash", format!("git commit -m '{}'", message)),
        AgentAction::WebSearch { queries } => {
            if queries.len() == 1 { ("Web Search", queries[0].0.clone()) }
            else { ("Web Search", format!("{} queries", queries.len())) }
        }
        AgentAction::WebFetch { url } => ("Web Fetch", url.clone()),
    };
    ActionDisplay {
        action_type: action_type.to_string(),
        target,
        result: AgentActionResult::Error { error: error.to_string() },
        preview: None,
        line_count: None,
        file_content: None,
        duration_seconds: None,
        targets: None,
        item_count: None,
        failed_items: None,
    }
}

fn build_action_display_with_timing(
    action: &AgentAction,
    output: &str,
    duration_seconds: Option<f64>,
) -> ActionDisplay {
    match action {
        AgentAction::WriteFile { path, content } => {
            let line_count = content.lines().count();
            ActionDisplay {
                action_type: "Write".to_string(),
                target: path.clone(),
                result: AgentActionResult::Success {
                    output: output.to_string(),
                },
                preview: None,
                line_count: Some(line_count),
                file_content: Some(content.clone()),
                duration_seconds: None,
                targets: None,
                item_count: None,
                failed_items: None,
            }
        }
        AgentAction::EditFile { path, old_string, new_string } => {
            let added = new_string.lines().count();
            let removed = old_string.lines().count();
            ActionDisplay {
                action_type: "Edit".to_string(),
                target: path.clone(),
                result: AgentActionResult::Success {
                    output: output.to_string(),
                },
                preview: Some(format!("Added {} lines, removed {} lines", added, removed)),
                line_count: Some(added + removed),
                file_content: Some(output.to_string()),
                duration_seconds: None,
                targets: None,
                item_count: None,
                failed_items: None,
            }
        }
        AgentAction::ReadFile { paths } => {
            let line_count = output.lines().count();
            if paths.len() == 1 {
                ActionDisplay {
                    action_type: "Read".to_string(),
                    target: paths[0].clone(),
                    result: AgentActionResult::Success {
                        output: output.to_string(),
                    },
                    preview: Some(truncate_output(output, 3)),
                    line_count: Some(line_count),
                    file_content: None,
                    duration_seconds,
                    targets: None,
                    item_count: None,
                    failed_items: None,
                }
            } else {
                ActionDisplay {
                    action_type: "Read".to_string(),
                    target: format!("{} files", paths.len()),
                    result: AgentActionResult::Success {
                        output: output.to_string(),
                    },
                    preview: Some(truncate_output(output, 5)),
                    line_count: Some(line_count),
                    file_content: None,
                    duration_seconds,
                    targets: Some(paths.clone()),
                    item_count: Some(paths.len()),
                    failed_items: None,
                }
            }
        }
        AgentAction::ExecuteCommand { command, .. } => ActionDisplay {
            action_type: "Bash".to_string(),
            target: command.clone(),
            result: AgentActionResult::Success {
                output: output.to_string(),
            },
            preview: Some(truncate_output(output, 5)),
            line_count: Some(output.lines().count()),
            file_content: None,
            duration_seconds,
            targets: None,
            item_count: None,
            failed_items: None,
        },
        AgentAction::DeleteFile { path } => ActionDisplay {
            action_type: "Delete".to_string(),
            target: path.clone(),
            result: AgentActionResult::Success {
                output: output.to_string(),
            },
            preview: None,
            line_count: None,
            file_content: None,
            duration_seconds: None,
            targets: None,
            item_count: None,
            failed_items: None,
        },
        AgentAction::CreateDirectory { path } => ActionDisplay {
            action_type: "Bash".to_string(),
            target: format!("mkdir -p {}", path),
            result: AgentActionResult::Success {
                output: output.to_string(),
            },
            preview: None,
            line_count: None,
            file_content: None,
            duration_seconds: None,
            targets: None,
            item_count: None,
            failed_items: None,
        },
        AgentAction::GitDiff { paths } => {
            let path_str = if paths.len() == 1 {
                paths[0].clone().unwrap_or_else(|| ".".to_string())
            } else {
                paths.iter()
                    .map(|p| p.clone().unwrap_or_else(|| ".".to_string()))
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            ActionDisplay {
                action_type: "Bash".to_string(),
                target: format!("git diff {}", path_str),
                result: AgentActionResult::Success {
                    output: output.to_string(),
                },
                preview: Some(truncate_output(output, 10)),
                line_count: Some(output.lines().count()),
                file_content: None,
                duration_seconds,
                targets: if paths.len() > 1 {
                    Some(paths.iter().map(|p| p.clone().unwrap_or_else(|| "*".to_string())).collect())
                } else {
                    None
                },
                item_count: if paths.len() > 1 { Some(paths.len()) } else { None },
                failed_items: None,
            }
        }
        AgentAction::GitStatus => ActionDisplay {
            action_type: "Bash".to_string(),
            target: "git status".to_string(),
            result: AgentActionResult::Success {
                output: output.to_string(),
            },
            preview: Some(truncate_output(output, 10)),
            line_count: Some(output.lines().count()),
            file_content: None,
            duration_seconds,
            targets: None,
            item_count: None,
            failed_items: None,
        },
        AgentAction::GitCommit { message, .. } => ActionDisplay {
            action_type: "Bash".to_string(),
            target: format!("git commit -m '{}'", message),
            result: AgentActionResult::Success {
                output: output.to_string(),
            },
            preview: Some(truncate_output(output, 3)),
            line_count: None,
            file_content: None,
            duration_seconds: None,
            targets: None,
            item_count: None,
            failed_items: None,
        },
        AgentAction::WebSearch { queries } => {
            // Detect if this is an error (no [SEARCH_RESULTS] marker means error or empty)
            let is_error = !output.contains("[SEARCH_RESULTS]");
            let result_count = output.matches("Title:").count();
            let preview = if is_error {
                Some(truncate_output(output, 2))
            } else {
                Some(format!("Fetched {} search results", result_count))
            };
            if queries.len() == 1 {
                ActionDisplay {
                    action_type: "Web Search".to_string(),
                    target: queries[0].0.clone(),
                    result: AgentActionResult::Success {
                        output: output.to_string(),
                    },
                    preview,
                    line_count: Some(result_count),
                    file_content: None,
                    duration_seconds,
                    targets: None,
                    item_count: None,
                    failed_items: None,
                }
            } else {
                ActionDisplay {
                    action_type: "Web Search".to_string(),
                    target: format!("{} queries", queries.len()),
                    result: AgentActionResult::Success {
                        output: output.to_string(),
                    },
                    preview,
                    line_count: Some(result_count),
                    file_content: None,
                    duration_seconds,
                    targets: Some(queries.iter().map(|(q, _)| q.clone()).collect()),
                    item_count: Some(queries.len()),
                    failed_items: None,
                }
            }
        }
        AgentAction::WebFetch { url } => {
            let content_len = output.lines().count();
            ActionDisplay {
                action_type: "Web Fetch".to_string(),
                target: url.clone(),
                result: AgentActionResult::Success {
                    output: output.to_string(),
                },
                preview: Some(truncate_output(output, 3)),
                line_count: Some(content_len),
                file_content: None,
                duration_seconds,
                targets: None,
                item_count: None,
                failed_items: None,
            }
        }
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
