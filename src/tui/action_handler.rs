use anyhow::Result;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::agents::{
    self, execute_action, ActionDisplay, ActionResult as AgentActionResult, AgentAction,
};
use crate::models::{ChatMessage, MessageRole, ModelConfig, StreamCallback, ToolCall};
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
    /// Whether the execution succeeded
    pub success: bool,
}

/// Execute a list of agent actions
///
/// Actions are executed sequentially without confirmation (Full YOLO mode).
pub async fn execute_actions(
    app: &mut App,
    actions: Vec<AgentAction>,
    tx: &mpsc::Sender<String>,
) -> Result<()> {
    for action in actions {
        let action_clone = action.clone();

        // Execute action directly
        match execute_action(&action).await {
            agents::ActionResult::Success { output } => {
                handle_action_success(app, &action_clone, output, tx).await?;
            }
            agents::ActionResult::Error { error } => {
                // Add action display to show the failed action in chat
                let action_display = build_action_display(&action_clone, &error);
                if let Some(last_msg) = app
                    .session_state
                    .messages
                    .iter_mut()
                    .rev()
                    .find(|m| matches!(m.role, MessageRole::Assistant))
                {
                    last_msg.actions.push(action_display);
                }

                // Use unified error display (status bar + chat)
                app.display_error_simple(&error);
            }
        }
    }

    Ok(())
}

/// Handle successful action execution
///
/// Different actions require different post-execution handling:
/// - ReadFile: Triggers feedback loop to send contents back to model
/// - WriteFile: Updates context with new file contents
/// - DeleteFile: Updates context to remove file
/// - Other actions: Just show success status
///
/// Also builds ActionDisplay and attaches to current message for UI rendering
async fn handle_action_success(
    app: &mut App,
    action: &AgentAction,
    output: String,
    tx: &mpsc::Sender<String>,
) -> Result<()> {
    // Build ActionDisplay for UI rendering
    let action_display = build_action_display(action, &output);

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

    // Perform action-specific post-processing
    match action {
        // Actions that need multi-step follow-through: Trigger unified feedback loop
        AgentAction::ReadFile { paths } => {
            // For single file reads, check if binary
            if paths.len() == 1 && crate::agents::is_binary_file(&paths[0]) {
                match crate::agents::read_binary_file(&paths[0]) {
                    Ok(base64_data) => {
                        trigger_multimodal_feedback_loop(app, action, base64_data, tx).await;
                    }
                    Err(e) => {
                        app.add_message(
                            MessageRole::Assistant,
                            format!("Failed to read binary file: {}", e),
                        );
                    }
                }
            } else {
                trigger_feedback_loop(app, action, output, tx).await;
            }
        }
        AgentAction::WebSearch { .. } => {
            trigger_feedback_loop(app, action, output, tx).await;
        }
        AgentAction::ExecuteCommand { .. } => {
            trigger_feedback_loop(app, action, output, tx).await;
        }
        AgentAction::GitDiff { .. } => {
            trigger_feedback_loop(app, action, output, tx).await;
        }
        AgentAction::GitStatus => {
            trigger_feedback_loop(app, action, output, tx).await;
        }

        // These actions are handled by the tool execution, nothing extra needed
        AgentAction::WriteFile { .. } => {}
        AgentAction::DeleteFile { .. } => {}
        AgentAction::CreateDirectory { .. } => {}
        AgentAction::GitCommit { .. } => {}
    }

    Ok(())
}

/// Build an ActionDisplay from an action and its output
fn build_action_display(action: &AgentAction, output: &str) -> ActionDisplay {
    build_action_display_with_timing(action, output, None)
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
                    action_type: "ReadFiles".to_string(),
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
            action_type: "CreateDir".to_string(),
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
        AgentAction::GitDiff { paths } => {
            if paths.len() == 1 {
                ActionDisplay {
                    action_type: "GitDiff".to_string(),
                    target: paths[0].clone().unwrap_or_else(|| ".".to_string()),
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
                }
            } else {
                ActionDisplay {
                    action_type: "GitDiffs".to_string(),
                    target: format!("{} paths", paths.len()),
                    result: AgentActionResult::Success {
                        output: output.to_string(),
                    },
                    preview: Some(truncate_output(output, 10)),
                    line_count: Some(output.lines().count()),
                    file_content: None,
                    duration_seconds,
                    targets: Some(
                        paths
                            .iter()
                            .map(|p| p.clone().unwrap_or_else(|| "*".to_string()))
                            .collect(),
                    ),
                    item_count: Some(paths.len()),
                    failed_items: None,
                }
            }
        }
        AgentAction::GitStatus => ActionDisplay {
            action_type: "GitStatus".to_string(),
            target: ".".to_string(),
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
            action_type: "GitCommit".to_string(),
            target: message.clone(),
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
            let result_count = output.matches("Title:").count();
            if queries.len() == 1 {
                ActionDisplay {
                    action_type: "WebSearch".to_string(),
                    target: queries[0].0.clone(),
                    result: AgentActionResult::Success {
                        output: output.to_string(),
                    },
                    preview: Some(format!("Fetched {} search results", result_count)),
                    line_count: Some(result_count),
                    file_content: None,
                    duration_seconds,
                    targets: None,
                    item_count: None,
                    failed_items: None,
                }
            } else {
                ActionDisplay {
                    action_type: "WebSearches".to_string(),
                    target: format!("{} queries", queries.len()),
                    result: AgentActionResult::Success {
                        output: output.to_string(),
                    },
                    preview: Some(format!("Fetched {} search results", result_count)),
                    line_count: Some(result_count),
                    file_content: None,
                    duration_seconds,
                    targets: Some(queries.iter().map(|(q, _)| q.clone()).collect()),
                    item_count: Some(queries.len()),
                    failed_items: None,
                }
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

/// Build feedback prompt for action results based on action type
fn build_feedback_prompt(action: &AgentAction, output: &str) -> String {
    match action {
        AgentAction::ReadFile { paths } => {
            if paths.len() == 1 {
                format!(
                    "I've successfully read the file '{}'. Here are its contents:\n\n{}\n\nPlease explain what this file contains and how it's relevant to the user's request.",
                    paths[0], output
                )
            } else {
                format!(
                    "I've successfully read {} files:\n\n{}\n\nPlease analyze these files together as a cohesive system:\n1. What patterns do you see across files?\n2. What's the overall flow or architecture?\n3. Are there inconsistencies or improvements needed?\n4. What could be optimized?",
                    paths.len(), output
                )
            }
        }
        AgentAction::WebSearch { queries } => {
            if queries.len() == 1 {
                format!(
                    "Here are the web search results for '{}':\n\n{}\n\nPlease analyze these results and respond to the user's original question. Summarize the key findings with [source: URL] citations, dates, and author information where available.",
                    queries[0].0, output
                )
            } else {
                format!(
                    "I've completed {} web searches:\n\n{}\n\nPlease analyze these results together and respond to the user's original question. Synthesize the findings with [source: URL] citations, dates, and author information where available.",
                    queries.len(), output
                )
            }
        }
        AgentAction::ExecuteCommand { command, .. } => {
            format!(
                "I executed the command '{}'. Here's the output:\n\n{}\n\nPlease interpret these results and explain what they mean in the context of the user's request.",
                command, output
            )
        }
        AgentAction::GitDiff { paths } => {
            if paths.len() == 1 {
                let context = if let Some(p) = &paths[0] {
                    format!("for {}", p)
                } else {
                    "for the repository".to_string()
                };
                format!(
                    "Here's the git diff {}:\n\n{}\n\nPlease analyze these changes and explain their implications.",
                    context, output
                )
            } else {
                format!(
                    "Here are {} git diffs:\n\n{}\n\nPlease analyze these changes together and explain their collective implications and how they work together.",
                    paths.len(), output
                )
            }
        }
        AgentAction::GitStatus => {
            format!(
                "Here's the current repository status:\n\n{}\n\nPlease explain the repository state clearly to the user.",
                output
            )
        }
        _ => String::new(),
    }
}

/// Trigger the unified multi-step feedback loop for action follow-through
async fn trigger_feedback_loop(
    app: &mut App,
    action: &AgentAction,
    output: String,
    tx: &mpsc::Sender<String>,
) {
    let feedback_prompt = build_feedback_prompt(action, &output);
    if feedback_prompt.is_empty() {
        return;
    }

    app.operation_state.pending_file_read = true;
    if app.operation_state.reading_file_status.is_none() {
        app.operation_state.reading_file_status = Some("Analyzing action results...".to_string());
        app.status_state.status_timestamp = Some(std::time::Instant::now());
    }

    app.current_response.clear();
    app.add_message(MessageRole::System, feedback_prompt.clone());
    let messages = app.build_message_history();

    let model = app.model_state.model.clone();
    let tx_clone = tx.clone();
    let tx_done = tx.clone();

    let handle = tokio::spawn(async move {
        let config = ModelConfig::default();
        let callback: StreamCallback = Arc::new(move |chunk| {
            let _ = tx_clone.try_send(chunk.to_string());
        });

        let model = model.write().await;
        match model
            .chat(&messages, &config, Some(callback))
            .await
        {
            Ok(_) => {
                let _ = tx_done.send("[DONE]:[FEEDBACK_COMPLETE]".to_string()).await;
            }
            Err(e) => {
                let error_json = e.to_channel_message();
                let _ = tx_done.send(format!("[ERROR_JSON]:{}", error_json)).await;
            }
        }
    });

    app.start_generation(handle.abort_handle());
}

/// Trigger multimodal feedback loop for binary files (PDFs, images)
async fn trigger_multimodal_feedback_loop(
    app: &mut App,
    action: &AgentAction,
    base64_data: String,
    tx: &mpsc::Sender<String>,
) {
    let feedback_prompt = match action {
        AgentAction::ReadFile { paths } => {
            if paths.len() == 1 {
                format!(
                    "I've successfully read the file '{}'. Please analyze this document and tell the user what it contains.",
                    paths[0]
                )
            } else {
                format!(
                    "I've successfully read {} files. Please analyze these documents and tell the user what they contain.",
                    paths.len()
                )
            }
        }
        _ => return,
    };

    app.operation_state.pending_file_read = true;
    if app.operation_state.reading_file_status.is_none() {
        app.operation_state.reading_file_status = Some("Analyzing document...".to_string());
        app.status_state.status_timestamp = Some(std::time::Instant::now());
    }

    app.current_response.clear();

    let mut messages = app.build_message_history();
    let feedback_message = ChatMessage {
        role: MessageRole::System,
        content: feedback_prompt,
        timestamp: chrono::Local::now(),
        actions: Vec::new(),
        thinking: None,
        images: Some(vec![base64_data]),
        tool_calls: None,
        tool_call_id: None,
        tool_name: None,
    };
    messages.push(feedback_message);

    let model = app.model_state.model.clone();
    let tx_clone = tx.clone();
    let tx_done = tx.clone();

    let handle = tokio::spawn(async move {
        let config = ModelConfig::default();
        let callback: StreamCallback = Arc::new(move |chunk| {
            let _ = tx_clone.try_send(chunk.to_string());
        });

        let model = model.write().await;
        match model
            .chat(&messages, &config, Some(callback))
            .await
        {
            Ok(_) => {
                let _ = tx_done.send("[DONE]:[FEEDBACK_COMPLETE]".to_string()).await;
            }
            Err(e) => {
                let error_json = e.to_channel_message();
                let _ = tx_done.send(format!("[ERROR_JSON]:{}", error_json)).await;
            }
        }
    });

    app.start_generation(handle.abort_handle());
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
                    success: false,
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
                    success: true,
                });
            }
            agents::ActionResult::Error { error } => {
                let action_display = build_action_display(&action_clone, &error);
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
                    success: false,
                });
            }
        }
    }

    results
}
