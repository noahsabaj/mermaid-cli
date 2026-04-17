//! Shared agent loop for tool-calling models.
//!
//! Used by **non-interactive mode** (`SilentObserver`), **sub-agents**
//! (`SubagentObserver`), and the **TUI** (`TuiObserver`). The observer
//! pattern lets each consumer plug in its own model-call and subagent-
//! handling strategies: the default trait methods do simple sync/join
//! work (good for non-interactive and subagents); the TUI overrides
//! `call_model` and `run_subagents` to drive channel-based streaming
//! and live rendering without forking the core loop.
//!
//! The trait is `Send`-bounded so futures returned by its default
//! methods are Send (required by `tokio::spawn` in subagent code).
//! The TUI observer is also Send: it holds `&mut Terminal<...>` and
//! `&mut App`, both of which are Send since `io::Stdout` and the
//! tokio sync primitives inside App are Send.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::RwLock;

use crate::agents::{
    ActionResult as AgentActionResult, AgentAction, SubagentProgress, SubagentResult,
    collect_subagent_results, execute_action, format_subagent_tool_result, spawn_subagents,
};
use crate::models::{ChatMessage, Model, ModelConfig, StreamCallback, StreamEvent, ToolCall};
use crate::utils::MutexExt;

/// Default maximum iterations for the agent loop
pub const MAX_AGENT_ITERATIONS: usize = 25;

/// Output of a single model call, returned from the `call_model` hook.
#[derive(Debug, Clone, Default)]
pub struct ModelCallOutput {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub tokens: usize,
}

/// How the agent loop communicates with its environment.
///
/// The six small sync hooks (`check_interrupt`, `on_status`, etc.) are
/// called between steps. The two async hooks (`call_model`,
/// `run_subagents`) do the heavy lifting and have default
/// implementations suitable for non-interactive use — the TUI overrides
/// them to thread channel-based streaming and live rendering through
/// the shared loop without forking it.
#[async_trait]
pub trait AgentObserver: Send {
    /// Called between steps to check for user interruption or injected messages.
    /// Returns `LoopControl::Continue` to proceed, `Interrupt` to stop,
    /// or `InjectMessage(text)` to redirect the agent with new user input.
    fn check_interrupt(&mut self) -> LoopControl;

    /// Called when the loop status changes (e.g., "Iteration 3 - executing tools")
    fn on_status(&mut self, message: &str);

    /// Called after a tool call is executed.
    ///
    /// Observers may use this to mirror tool results into side storage
    /// (e.g., the TUI commits the tool message to session_state for
    /// live rendering) — but the loop ALSO appends the tool message to
    /// its own `messages` vec so the next model call sees it. When
    /// that vec IS the side storage (TUI passes `&mut session_state
    /// .messages`), both paths land on the same data.
    fn on_tool_result(
        &mut self,
        tool_name: &str,
        tool_call_id: &str,
        action: &AgentAction,
        result: &AgentActionResult,
    );

    /// Called when the model returns an error.
    fn on_error(&mut self, error: &str);

    /// Called when model generation starts (for status tracking).
    fn on_generation_start(&mut self);

    /// Called when model generation completes with token count.
    fn on_generation_complete(&mut self, tokens: usize);

    /// Called whenever the loop appends a message to its `messages` vec.
    ///
    /// Default: no-op. The TUI overrides this to mirror the message into
    /// `app.session_state.messages` (its UI source of truth) without
    /// requiring run_agent_loop to borrow session_state mutably (which
    /// would alias through the observer).
    fn on_message_appended(&mut self, _msg: &ChatMessage) {}

    /// Call the model and return its response.
    ///
    /// Default: direct `model.chat_typed()` with a typed-event accumulator —
    /// matches what non-interactive mode and subagents need. Reasoning
    /// chunks are accumulated separately so they don't pollute the
    /// returned text content. Tool calls are deduped against the response
    /// (the adapter populates `ModelResponse.tool_calls` as a fallback if
    /// the typed stream emitted none).
    ///
    /// Override for: channel-based streaming, mid-stream interrupt, UI
    /// rendering between chunks.
    async fn call_model(
        &mut self,
        model: Arc<RwLock<Box<dyn Model>>>,
        messages: &[ChatMessage],
        config: &ModelConfig,
    ) -> Result<ModelCallOutput> {
        let text = Arc::new(std::sync::Mutex::new(String::new()));
        let typed_tool_calls = Arc::new(std::sync::Mutex::new(Vec::<ToolCall>::new()));
        let text_clone = Arc::clone(&text);
        let tool_clone = Arc::clone(&typed_tool_calls);
        let callback: StreamCallback = Arc::new(move |event| match event {
            StreamEvent::Text(chunk) => {
                text_clone.lock_mut_safe().push_str(&chunk);
            },
            StreamEvent::ToolCall(tc) => {
                tool_clone.lock_mut_safe().push(tc);
            },
            // Reasoning chunks dropped — the loop doesn't surface
            // reasoning to observers. `ModelResponse.thinking` (still
            // populated by the adapter) is available if needed.
            StreamEvent::Reasoning(_) | StreamEvent::Done { .. } => {},
        });

        let model_guard = model.read().await;
        let response = model_guard
            .chat(messages, config, Some(callback))
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;

        let streamed_text = text.lock_mut_safe().clone();
        let content = if !streamed_text.is_empty() {
            streamed_text
        } else {
            response.content.clone()
        };
        let tokens = response.usage.map(|u| u.total_tokens).unwrap_or(0);
        let streamed_tool_calls = std::mem::take(&mut *typed_tool_calls.lock_mut_safe());
        let tool_calls = if !streamed_tool_calls.is_empty() {
            streamed_tool_calls
        } else {
            response.tool_calls.unwrap_or_default()
        };

        Ok(ModelCallOutput {
            content,
            tool_calls,
            tokens,
        })
    }

    /// Run a batch of subagents to completion and return their results.
    ///
    /// Default: `spawn_subagents` + `collect_subagent_results` with no
    /// rendering between polls. Override for live progress rendering.
    async fn run_subagents(
        &mut self,
        specs: Vec<(String, String)>,
        model: Arc<RwLock<Box<dyn Model>>>,
        config: &ModelConfig,
    ) -> Vec<SubagentResult> {
        let progress = Arc::new(Mutex::new(Vec::<SubagentProgress>::new()));
        let (handles, overflow) = spawn_subagents(specs, model, config, Arc::clone(&progress));
        collect_subagent_results(handles, overflow).await
    }
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
/// Used by non-interactive mode, sub-agents, AND the TUI. The observer's
/// `call_model` and `run_subagents` hooks let each caller customize the
/// execution strategy without duplicating the loop skeleton.
pub async fn run_agent_loop(
    model: Arc<RwLock<Box<dyn Model>>>,
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
                let user_msg = ChatMessage::user(msg);
                observer.on_message_appended(&user_msg);
                messages.push(user_msg);
                current_tool_calls.clear();
                // Falls through to model call below
            },
        }

        // If tool calls were cleared by InjectMessage, skip execution and go to model call
        if !current_tool_calls.is_empty() {
            // Every caller pre-pushes the assistant message with its tool_calls
            // already set (via `ChatMessage::with_tool_calls`), and subsequent
            // iterations push fresh assistants with_tool_calls below — so no
            // in-place mutation of the previous assistant is needed here.

            // Partition into regular tool calls and agent tool calls
            let (regular_calls, agent_calls): (Vec<_>, Vec<_>) = current_tool_calls
                .iter()
                .partition(|tc| tc.function.name != "agent");

            // Execute regular tool calls first (sequential, as before).
            // The fallback id includes the in-iteration index so two calls
            // to the same tool (e.g., two `read_file`s in parallel) don't
            // collide when the model omits the `id` field.
            for (idx, tc) in regular_calls.iter().enumerate() {
                let tool_call_id = tc
                    .id
                    .clone()
                    .unwrap_or_else(|| format!("call_{}_{}_{}", iteration, idx, tc.function.name));
                let tool_name = tc.function.name.clone();

                let agent_action = match tc.to_agent_action() {
                    Ok(action) => action,
                    Err(e) => {
                        let error_msg = format!("Error: {}", e);
                        let tool_msg = ChatMessage::tool(&tool_call_id, &tool_name, &error_msg);
                        observer.on_message_appended(&tool_msg);
                        messages.push(tool_msg);
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
                observer.on_message_appended(&tool_msg);
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

            // Execute agent tool calls in parallel via the observer hook
            // (default: join_all; TUI: poll with live rendering).
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
                    let subagent_results = observer
                        .run_subagents(agent_specs, Arc::clone(&model), config)
                        .await;

                    for (i, result) in subagent_results.iter().enumerate() {
                        let tool_call_id = agent_calls
                            .get(i)
                            .and_then(|tc| tc.id.clone())
                            .unwrap_or_else(|| format!("call_agent_{}_{}", iteration, i));
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

                        let tool_msg = ChatMessage::tool(&tool_call_id, &tool_name, &output);
                        observer.on_message_appended(&tool_msg);
                        messages.push(tool_msg);
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
                let user_msg = ChatMessage::user(msg);
                observer.on_message_appended(&user_msg);
                messages.push(user_msg);
            },
            LoopControl::Continue => {},
        }

        // Call model via the observer hook (default: direct chat; TUI:
        // channel-based streaming with live rendering).
        observer.on_generation_start();
        let model_result = observer
            .call_model(Arc::clone(&model), messages, config)
            .await;

        match model_result {
            Ok(out) => {
                total_tokens += out.tokens;
                observer.on_generation_complete(out.tokens);

                let new_tool_calls = out.tool_calls;

                // Add assistant message to history
                if !out.content.is_empty() || !new_tool_calls.is_empty() {
                    let msg = ChatMessage::assistant(out.content.clone())
                        .with_tool_calls(new_tool_calls.clone());
                    observer.on_message_appended(&msg);
                    messages.push(msg);
                }

                if new_tool_calls.is_empty() {
                    // No more tool calls -- agent loop complete
                    final_response = out.content;
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
