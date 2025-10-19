use anyhow::Result;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::agents::{
    self, ActionDisplay, ActionResult as AgentActionResult, AgentAction, ModeAwareExecutor,
};
use crate::models::{MessageRole, ModelConfig, StreamCallback};
use crate::tui::{App, ConfirmationState, FileInfo};
use crate::utils::count_file_tokens;

/// Execute a list of agent actions
///
/// Actions are executed sequentially. If an action requires confirmation,
/// execution pauses and waits for user input (Alt+Y/N).
pub async fn execute_actions(
    app: &mut App,
    actions: Vec<AgentAction>,
    tx: &mpsc::Sender<String>,
) -> Result<()> {
    // Create mode-aware executor
    let mut executor = ModeAwareExecutor::new(app.operation_mode.clone());

    for action in actions {
        // Check if action needs confirmation
        if executor.needs_confirmation(&action) {
            // Create confirmation state for inline display
            let action_desc = executor.describe_action(&action);

            // Extract preview and file info for WriteFile actions
            let (preview_lines, file_info) = match &action {
                AgentAction::WriteFile { path, content } => {
                    let lines: Vec<String> =
                        content.lines().take(5).map(|s| s.to_string()).collect();
                    let info = FileInfo {
                        path: path.clone(),
                        size: content.len(),
                        exists: Path::new(path).exists(),
                        language: detect_language(path),
                    };
                    (lines, Some(info))
                },
                _ => (vec![], None),
            };

            // Set confirmation state
            app.confirmation_state = Some(ConfirmationState {
                action: action.clone(),
                action_description: action_desc,
                preview_lines,
                file_info,
                allow_always: matches!(action, AgentAction::WriteFile { .. }),
            });

            // Store executor for later use
            app.pending_action = Some(action);
            app.pending_executor = Some(executor);
            break; // Wait for user confirmation
        } else {
            // Clone action to check type after execution
            let action_clone = action.clone();

            // Execute action directly
            match executor.execute(action).await {
                Ok(agents::ActionResult::Success { output }) => {
                    // Handle action-specific success logic
                    handle_action_success(app, &action_clone, output, tx).await?;
                },
                Ok(agents::ActionResult::Error { error }) => {
                    app.set_status(format!("[FAILED] Action failed: {}", error));
                },
                Err(e) => {
                    app.set_status(format!("[ERROR] Error: {}", e));
                },
            }
        }
    }

    Ok(())
}

/// Confirm or reject a pending action
///
/// This is called when the user presses Alt+Y (approve) or Alt+N (reject).
pub async fn confirm_action(
    app: &mut App,
    approved: bool,
    tx: &mpsc::Sender<String>,
) -> Result<()> {
    if approved {
        // Approve and execute the action
        if let Some(confirmation) = app.confirmation_state.take() {
            app.set_status(format!("Executing: {}...", confirmation.action_description));

            // Get the executor from pending_executor
            if let Some(mut executor) = app.pending_executor.take() {
                let action_clone = confirmation.action.clone();

                // Execute the action
                match executor.execute(confirmation.action).await {
                    Ok(agents::ActionResult::Success { output }) => {
                        handle_action_success(app, &action_clone, output, tx).await?;
                    },
                    Ok(agents::ActionResult::Error { error }) => {
                        app.set_status(format!("[FAILED] Action failed: {}", error));
                    },
                    Err(e) => {
                        app.set_status(format!("[ERROR] Error: {}", e));
                    },
                }
            }
            app.pending_action = None;
        }
    } else {
        // Reject the action
        if app.confirmation_state.take().is_some() {
            app.set_status("Action skipped");
            app.pending_action = None;
            app.pending_executor = None;
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
        .messages
        .iter_mut()
        .rev()
        .find(|m| matches!(m.role, MessageRole::Assistant))
    {
        last_msg.actions.push(action_display);
    }

    // Perform action-specific post-processing
    match action {
        AgentAction::ReadFile { path } => {
            // Feedback loop: Send file contents back to model
            app.set_status(format!("[OK] File read: {}", path));

            // Set feedback tracking
            app.pending_file_read = true;
            if app.reading_file_status.is_none() {
                app.reading_file_status = Some(format!("Processing {}...", path));
                app.status_timestamp = Some(std::time::Instant::now());
            }

            app.is_generating = true;
            app.current_response.clear();

            // Create a prompt for the model to present the file contents
            let feedback_prompt = format!(
                "I've successfully read the file '{}'. Here are its contents:\n\n{}\n\nPlease present these contents to the user in a helpful way.",
                path, output
            );

            // Add feedback as system message and build history
            app.add_message(MessageRole::System, feedback_prompt.clone());
            let messages = app.build_message_history();

            // Send feedback to model
            let model = app.model.clone();
            let context = app.context.clone();
            let tx_clone = tx.clone();
            let tx_done = tx.clone();

            tokio::spawn(async move {
                let config = ModelConfig::default();
                let callback: StreamCallback = Arc::new(move |chunk| {
                    let _ = tx_clone.try_send(chunk.to_string());
                });

                let mut model = model.write().await;
                match model
                    .chat(&messages, &context, &config, Some(callback))
                    .await
                {
                    Ok(_) => {
                        // Clear feedback flags after completion
                        let _ = tx_done.send("[DONE]:[FEEDBACK_COMPLETE]".to_string()).await;
                    },
                    Err(e) => {
                        let _ = tx_done.send(format!("[ERROR]:{}", e)).await;
                    },
                }
            });
        },
        AgentAction::WriteFile { path, content } => {
            app.set_status(format!("[OK] {}", output));
            app.context.add_file(path.clone(), content.clone());
            // Use proper tokenizer for accurate count
            let tokens = count_file_tokens(content, &app.model_name);
            app.context.token_count += tokens;
        },
        AgentAction::DeleteFile { path } => {
            app.set_status(format!("[OK] {}", output));
            if let Some(content) = app.context.files.remove(path) {
                // Use proper tokenizer for accurate count
                let tokens = count_file_tokens(&content, &app.model_name);
                app.context.token_count = app.context.token_count.saturating_sub(tokens);
            }
        },
        AgentAction::WebSearch { query, .. } => {
            // Add search results to conversation so model can reference them
            app.set_status(format!("[OK] Search complete for: {}", query));
            app.add_message(MessageRole::Assistant, output.clone());
        },
        _ => {
            app.set_status(format!("[OK] {}", output));
        },
    }

    Ok(())
}

/// Build an ActionDisplay from an action and its output
fn build_action_display(action: &AgentAction, output: &str) -> ActionDisplay {
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
            }
        },
        AgentAction::ReadFile { path } => {
            let line_count = output.lines().count();
            ActionDisplay {
                action_type: "Read".to_string(),
                target: path.clone(),
                result: AgentActionResult::Success {
                    output: output.to_string(),
                },
                preview: Some(truncate_output(output, 3)),
                line_count: Some(line_count),
                file_content: None,
            }
        },
        AgentAction::ExecuteCommand { command, .. } => ActionDisplay {
            action_type: "Bash".to_string(),
            target: command.clone(),
            result: AgentActionResult::Success {
                output: output.to_string(),
            },
            preview: Some(truncate_output(output, 5)),
            line_count: Some(output.lines().count()),
            file_content: None,
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
        },
        AgentAction::GitDiff { path } => ActionDisplay {
            action_type: "GitDiff".to_string(),
            target: path.clone().unwrap_or_else(|| ".".to_string()),
            result: AgentActionResult::Success {
                output: output.to_string(),
            },
            preview: Some(truncate_output(output, 10)),
            line_count: Some(output.lines().count()),
            file_content: None,
        },
        AgentAction::GitStatus => ActionDisplay {
            action_type: "GitStatus".to_string(),
            target: ".".to_string(),
            result: AgentActionResult::Success {
                output: output.to_string(),
            },
            preview: Some(truncate_output(output, 10)),
            line_count: Some(output.lines().count()),
            file_content: None,
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
        },
        AgentAction::WebSearch { query, .. } => {
            let result_count = output.matches("Title:").count();
            ActionDisplay {
                action_type: "WebSearch".to_string(),
                target: query.clone(),
                result: AgentActionResult::Success {
                    output: output.to_string(),
                },
                preview: Some(format!("Fetched {} search results", result_count)),
                line_count: Some(result_count),
                file_content: None,
            }
        },
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

/// Detect programming language from file extension
///
/// Used to provide context for file previews in confirmation dialogs.
fn detect_language(path: &str) -> Option<String> {
    let ext = Path::new(path).extension().and_then(|e| e.to_str())?;

    match ext {
        "rs" => Some("Rust".to_string()),
        "py" => Some("Python".to_string()),
        "js" | "jsx" => Some("JavaScript".to_string()),
        "ts" | "tsx" => Some("TypeScript".to_string()),
        "go" => Some("Go".to_string()),
        "java" => Some("Java".to_string()),
        "c" => Some("C".to_string()),
        "cpp" | "cc" | "cxx" => Some("C++".to_string()),
        "h" | "hpp" => Some("C/C++ Header".to_string()),
        "rb" => Some("Ruby".to_string()),
        "php" => Some("PHP".to_string()),
        "swift" => Some("Swift".to_string()),
        "kt" => Some("Kotlin".to_string()),
        "scala" => Some("Scala".to_string()),
        "sh" | "bash" => Some("Shell".to_string()),
        "yaml" | "yml" => Some("YAML".to_string()),
        "toml" => Some("TOML".to_string()),
        "json" => Some("JSON".to_string()),
        "xml" => Some("XML".to_string()),
        "html" => Some("HTML".to_string()),
        "css" | "scss" | "sass" => Some("CSS".to_string()),
        "md" => Some("Markdown".to_string()),
        _ => None,
    }
}

/// Approve a plan and start executing it
pub async fn approve_plan(app: &mut App, tx: &mpsc::Sender<String>) -> Result<()> {
    if app.active_plan.is_none() {
        return Ok(());
    }

    app.start_plan_execution();

    // Execute the first action in the plan
    execute_plan_step(app, tx).await
}

/// Cancel a pending plan
pub fn cancel_plan(app: &mut App) {
    app.cancel_plan();
}

/// Execute the next step in a plan
/// This is called repeatedly as each action completes
pub async fn execute_plan_step(app: &mut App, tx: &mpsc::Sender<String>) -> Result<()> {
    // Check if plan still exists and get the next action
    let next_action = if let Some(plan) = app.active_plan.as_ref() {
        plan.next_pending_action().map(|(_, action)| action.clone())
    } else {
        None
    };

    if let Some(planned_action) = next_action {
        let action = planned_action.action.clone();
        let action_clone = action.clone();
        let mut executor = ModeAwareExecutor::new(app.operation_mode.clone());

        // Execute action directly in plan mode
        match executor.execute(action).await {
            Ok(agents::ActionResult::Success { output }) => {
                app.mark_plan_action_completed(Some(agents::ActionResult::Success {
                    output: output.clone(),
                }));

                // Handle action-specific post-processing
                handle_action_success(app, &action_clone, output, tx).await?;

                // Update plan display and get stats for status message
                if let Some(plan) = app.active_plan.as_mut() {
                    let stats = plan.stats();
                    if stats.is_complete() {
                        app.set_status(format!(
                            "Plan complete: {}/{} actions succeeded",
                            stats.completed, stats.total
                        ));
                    } else {
                        app.set_status(format!(
                            "Plan executing: {}/{}",
                            stats.completed + stats.failed + stats.skipped,
                            stats.total
                        ));
                    }
                }
            },
            Ok(agents::ActionResult::Error { error }) => {
                app.mark_plan_action_failed(error.clone());
                app.set_status(format!("Plan action failed: {}", error));
            },
            Err(e) => {
                app.mark_plan_action_failed(e.to_string());
                app.set_status(format!("Plan action error: {}", e));
            },
        }
    } else {
        // Plan is complete
        if let Some(plan) = app.active_plan.as_ref() {
            let stats = plan.stats();
            let message = if stats.has_failures() {
                format!(
                    "Plan completed with {} failures ({}/{} successful)",
                    stats.failed, stats.completed, stats.total
                )
            } else {
                format!("Plan completed successfully ({} actions)", stats.total)
            };
            app.set_status(message);
        }
    }

    Ok(())
}
