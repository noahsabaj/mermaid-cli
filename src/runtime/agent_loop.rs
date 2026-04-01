//! Shared agent loop for tool-calling models.
//!
//! Used by **non-interactive mode** (`SilentObserver`) and **sub-agents**
//! (`SubagentObserver`). The TUI has its own agent loop in
//! `tui::loop_coordinator::run_agent_loop` because it needs direct `Terminal`
//! access for live rendering and interrupt handling — `Terminal` is not `Send`
//! so it cannot be passed through the `AgentObserver` trait.
//!
//! The `AgentObserver` trait allows each non-TUI consumer to provide its own
//! I/O behavior (interruption checks, status logging, tool result handling).

use std::sync::{Arc, Mutex};

use anyhow::Result;

use crate::agents::{
    ActionResult as AgentActionResult, AgentAction, SubagentProgress, collect_subagent_results,
    execute_action, format_subagent_tool_result, spawn_subagents,
};
use crate::models::{ChatMessage, Model, ModelConfig, StreamCallback, ToolCall};
use crate::utils::MutexExt;

/// Default maximum iterations for the agent loop
pub const MAX_AGENT_ITERATIONS: usize = 25;

/// How the agent loop communicates with its environment
pub trait AgentObserver: Send {
    /// Called between steps to check for user interruption or injected messages.
    /// Returns `LoopControl::Continue` to proceed, `Interrupt` to stop,
    /// or `InjectMessage(text)` to redirect the agent with new user input.
    fn check_interrupt(&mut self) -> LoopControl;

    /// Called when the loop status changes (e.g., "Iteration 3 - executing tools")
    fn on_status(&mut self, message: &str);

    /// Called after a tool call is executed
    fn on_tool_result(
        &mut self,
        tool_name: &str,
        tool_call_id: &str,
        action: &AgentAction,
        result: &AgentActionResult,
    );

    /// Called when the model returns an error
    fn on_error(&mut self, error: &str);

    /// Called when model generation starts (for status tracking)
    fn on_generation_start(&mut self);

    /// Called when model generation completes with token count
    fn on_generation_complete(&mut self, tokens: usize);
}

/// Control flow for the agent loop
pub enum LoopControl {
    /// Continue normally
    Continue,
    /// User interrupted (Esc, Ctrl+C)
    Interrupt,
    /// User injected a new message that redirects the agent
    InjectMessage(String),
}

/// Result of running the agent loop
pub struct AgentLoopResult {
    /// The model's final text response (from the last iteration with no tool calls)
    pub final_response: String,
    /// Number of iterations completed
    pub iterations: usize,
    /// Whether the loop was interrupted by the user
    pub interrupted: bool,
    /// All tool execution results across iterations
    pub tool_results: Vec<ToolExecutionResult>,
    /// Total tokens used across all model calls
    pub total_tokens: usize,
}

/// Result of a single tool execution
#[derive(Debug, Clone)]
pub struct ToolExecutionResult {
    pub tool_call_id: String,
    pub tool_name: String,
    pub action: AgentAction,
    pub success: bool,
    pub output: String,
    pub images: Option<Vec<String>>,
}

/// Run the agent loop: execute tool calls, feed results back, repeat.
///
/// Used by non-interactive mode and sub-agents. The TUI has its own
/// implementation in `tui::loop_coordinator::run_agent_loop` — see that
/// function's documentation for the rationale.
pub async fn run_agent_loop(
    model: Arc<tokio::sync::RwLock<Box<dyn Model>>>,
    config: &ModelConfig,
    messages: &mut Vec<ChatMessage>,
    initial_tool_calls: Vec<ToolCall>,
    observer: &mut dyn AgentObserver,
    max_iterations: usize,
) -> Result<AgentLoopResult> {
    let mut current_tool_calls = initial_tool_calls;
    let mut iteration = 0;
    let mut all_tool_results = Vec::new();
    let mut total_tokens = 0;
    let mut final_response = String::new();
    let mut interrupted = false;

    while !current_tool_calls.is_empty() {
        iteration += 1;
        if iteration > max_iterations {
            observer.on_status(&format!(
                "Agent loop exceeded {} iterations",
                max_iterations
            ));
            break;
        }

        observer.on_status(&format!("Agent loop iteration {}", iteration));

        // Check for interruption or injected messages
        match observer.check_interrupt() {
            LoopControl::Continue => {},
            LoopControl::Interrupt => {
                interrupted = true;
                break;
            },
            LoopControl::InjectMessage(msg) => {
                // User typed a message during the loop -- redirect agent
                observer.on_status("Processing queued message...");
                messages.push(ChatMessage::user(msg));
                current_tool_calls.clear();
                // Falls through to model call below
            },
        }

        // If tool calls were cleared by InjectMessage, skip execution and go to model call
        if !current_tool_calls.is_empty() {
            // Add tool_calls to the last assistant message in history
            if let Some(last_assistant) = messages
                .iter_mut()
                .rev()
                .find(|m| matches!(m.role, crate::models::MessageRole::Assistant))
            {
                last_assistant.tool_calls = Some(current_tool_calls.clone());
            }

            // Partition into regular tool calls and agent tool calls
            let (regular_calls, agent_calls): (Vec<_>, Vec<_>) = current_tool_calls
                .iter()
                .partition(|tc| tc.function.name != "agent");

            // Execute regular tool calls first (sequential, as before)
            for tc in &regular_calls {
                let tool_call_id = tc
                    .id
                    .clone()
                    .unwrap_or_else(|| format!("call_{}_{}", iteration, tc.function.name));
                let tool_name = tc.function.name.clone();

                let agent_action = match tc.to_agent_action() {
                    Ok(action) => action,
                    Err(e) => {
                        let error_msg = format!("Error: {}", e);
                        messages.push(ChatMessage::tool(&tool_call_id, &tool_name, &error_msg));
                        all_tool_results.push(ToolExecutionResult {
                            tool_call_id,
                            tool_name,
                            action: AgentAction::ParseError {
                                message: error_msg.clone(),
                            },
                            success: false,
                            output: error_msg,
                            images: None,
                        });
                        continue;
                    },
                };

                let result = execute_action(&agent_action).await;
                let (success, output, images) = match &result {
                    AgentActionResult::Success { output, images } => {
                        (true, output.clone(), images.clone())
                    },
                    AgentActionResult::Error { error } => {
                        (false, format!("Error: {}", error), None)
                    },
                };

                observer.on_tool_result(&tool_name, &tool_call_id, &agent_action, &result);

                let mut tool_msg = ChatMessage::tool(&tool_call_id, &tool_name, &output);
                if let Some(ref imgs) = images {
                    tool_msg = tool_msg.with_images(imgs.clone());
                }
                messages.push(tool_msg);
                all_tool_results.push(ToolExecutionResult {
                    tool_call_id,
                    tool_name,
                    action: agent_action,
                    success,
                    output,
                    images,
                });
            }

            // Execute agent tool calls in parallel (non-interactive: join_all directly)
            if !agent_calls.is_empty() {
                let agent_specs: Vec<(String, String)> = agent_calls
                    .iter()
                    .filter_map(|tc| match tc.to_agent_action() {
                        Ok(AgentAction::SpawnAgent {
                            prompt,
                            description,
                        }) => Some((prompt, description)),
                        _ => None,
                    })
                    .collect();

                if !agent_specs.is_empty() {
                    let progress = Arc::new(Mutex::new(Vec::<SubagentProgress>::new()));
                    let (handles, overflow) = spawn_subagents(
                        agent_specs,
                        Arc::clone(&model),
                        config,
                        Arc::clone(&progress),
                    );

                    let subagent_results = collect_subagent_results(handles, overflow).await;

                    for (i, result) in subagent_results.iter().enumerate() {
                        let tool_call_id = agent_calls
                            .get(i)
                            .and_then(|tc| tc.id.clone())
                            .unwrap_or_else(|| format!("call_agent_{}", i));
                        let tool_name = "agent".to_string();
                        let output = format_subagent_tool_result(result);

                        observer.on_tool_result(
                            &tool_name,
                            &tool_call_id,
                            &AgentAction::SpawnAgent {
                                prompt: String::new(),
                                description: result.description.clone(),
                            },
                            &if result.success {
                                AgentActionResult::Success {
                                    output: output.clone(),
                                    images: None,
                                }
                            } else {
                                AgentActionResult::Error {
                                    error: output.clone(),
                                }
                            },
                        );

                        messages.push(ChatMessage::tool(&tool_call_id, &tool_name, &output));
                        all_tool_results.push(ToolExecutionResult {
                            tool_call_id,
                            tool_name,
                            action: AgentAction::SpawnAgent {
                                prompt: String::new(),
                                description: result.description.clone(),
                            },
                            success: result.success,
                            output,
                            images: None,
                        });

                        total_tokens += result.tokens;
                    }
                }
            }

            observer.on_status(&format!(
                "Iteration {} - {} tool(s) executed, calling model...",
                iteration,
                current_tool_calls.len()
            ));
        }

        // Check for interruption before model call
        match observer.check_interrupt() {
            LoopControl::Interrupt => {
                interrupted = true;
                break;
            },
            LoopControl::InjectMessage(msg) => {
                messages.push(ChatMessage::user(msg));
            },
            LoopControl::Continue => {},
        }

        // Call model with updated history
        observer.on_generation_start();
        let response_text = Arc::new(std::sync::Mutex::new(String::new()));
        let response_clone = Arc::clone(&response_text);
        let callback: StreamCallback = Arc::new(move |chunk: &str| {
            let mut resp = response_clone.lock_mut_safe();
            resp.push_str(chunk);
        });

        let model_result = {
            let model = model.read().await;
            model.chat(messages, config, Some(callback)).await
        };

        match model_result {
            Ok(response) => {
                let content = {
                    let buf = response_text.lock_mut_safe();
                    if !buf.is_empty() {
                        buf.clone()
                    } else {
                        response.content.clone()
                    }
                };
                let tokens = response.usage.map(|u| u.total_tokens).unwrap_or(0);
                total_tokens += tokens;
                observer.on_generation_complete(tokens);

                let new_tool_calls = response.tool_calls.unwrap_or_default();

                // Add assistant message to history
                if !content.is_empty() || !new_tool_calls.is_empty() {
                    let msg = ChatMessage::assistant(content.clone())
                        .with_tool_calls(new_tool_calls.clone());
                    messages.push(msg);
                }

                if new_tool_calls.is_empty() {
                    // No more tool calls -- agent loop complete
                    final_response = content;
                    observer.on_status(&format!(
                        "Agent loop complete after {} iterations",
                        iteration
                    ));
                    break;
                } else {
                    current_tool_calls = new_tool_calls;
                }
            },
            Err(e) => {
                observer.on_error(&e.to_string());
                break;
            },
        }
    }

    Ok(AgentLoopResult {
        final_response,
        iterations: iteration,
        interrupted,
        tool_results: all_tool_results,
        total_tokens,
    })
}
