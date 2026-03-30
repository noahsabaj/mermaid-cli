use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::utils::MutexExt;

use crate::{
    agents::{ActionResult as AgentActionResult, AgentAction},
    app::Config,
    cli::OutputFormat,
    constants::{DEFAULT_MAX_TOKENS, DEFAULT_TEMPERATURE},
    models::{ChatMessage, Model, ModelConfig, ModelFactory},
    prompts,
};

use super::agent_loop::{self, AgentObserver, LoopControl, MAX_AGENT_ITERATIONS};

/// Result of a non-interactive run
#[derive(Debug, Serialize, Deserialize)]
pub struct NonInteractiveResult {
    /// The prompt that was executed
    pub prompt: String,
    /// The model's response
    pub response: String,
    /// Actions that were executed (if any)
    pub actions: Vec<ActionResult>,
    /// Any errors that occurred
    pub errors: Vec<String>,
    /// Metadata about the execution
    pub metadata: ExecutionMetadata,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ActionResult {
    /// Type of action (file_write, command, etc.)
    pub action_type: String,
    /// Target (file path or command)
    pub target: String,
    /// Whether the action was executed successfully
    pub success: bool,
    /// Output or error message
    pub output: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ExecutionMetadata {
    /// Model used
    pub model: String,
    /// Total tokens used
    pub tokens_used: Option<usize>,
    /// Execution time in milliseconds
    pub duration_ms: u128,
    /// Whether actions were executed
    pub actions_executed: bool,
}

/// Non-interactive runner for executing single prompts
pub struct NonInteractiveRunner {
    model: Arc<RwLock<Box<dyn Model>>>,
    no_execute: bool,
    max_tokens: Option<usize>,
}

impl NonInteractiveRunner {
    /// Create a new non-interactive runner
    pub async fn new(
        model_id: String,
        config: Config,
        no_execute: bool,
        max_tokens: Option<usize>,
    ) -> Result<Self> {
        // Create model instance
        let model = ModelFactory::create(&model_id, Some(&config)).await?;

        Ok(Self {
            model: Arc::new(RwLock::new(model)),
            no_execute,
            max_tokens,
        })
    }

    /// Execute a single prompt and return the result
    pub async fn execute(&self, prompt: String) -> Result<NonInteractiveResult> {
        let start_time = std::time::Instant::now();
        let mut errors = Vec::new();
        let mut total_tokens = 0;

        // Build initial messages
        let system_message = ChatMessage::system(prompts::get_system_prompt());
        let user_message = ChatMessage::user(prompt.clone());
        let mut messages = vec![system_message, user_message];

        // Build model config
        let model_name = {
            let model = self.model.read().await;
            model.name().to_string()
        };
        let model_config = ModelConfig {
            model: model_name.clone(),
            temperature: DEFAULT_TEMPERATURE,
            max_tokens: self.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
            thinking_enabled: Some(false),
            ..ModelConfig::default()
        };

        // First model call to get the initial response
        let response_text = Arc::new(std::sync::Mutex::new(String::new()));
        let response_clone = Arc::clone(&response_text);
        let callback = Arc::new(move |chunk: &str| {
            let mut resp = response_clone.lock_mut_safe();
            resp.push_str(chunk);
        });

        let result = {
            let model = self.model.read().await;
            model.chat(&messages, &model_config, Some(callback)).await
        };

        let (content, initial_tool_calls) = match result {
            Ok(response) => {
                let callback_content = response_text.lock_mut_safe().clone();
                let content = if !callback_content.is_empty() {
                    callback_content
                } else {
                    response.content
                };
                total_tokens += response.usage.map(|u| u.total_tokens).unwrap_or(0);
                let tool_calls = response.tool_calls.unwrap_or_default();
                (content, tool_calls)
            },
            Err(e) => {
                errors.push(format!("Model error: {}", e));
                let content = response_text.lock_mut_safe().clone();
                (content, vec![])
            },
        };

        // If no tool calls, return immediately
        if initial_tool_calls.is_empty() {
            let duration_ms = start_time.elapsed().as_millis();
            return Ok(NonInteractiveResult {
                prompt,
                response: content,
                actions: vec![],
                errors,
                metadata: ExecutionMetadata {
                    model: model_name,
                    tokens_used: Some(total_tokens),
                    duration_ms,
                    actions_executed: false,
                },
            });
        }

        // Add assistant message with tool calls to history
        let assistant_msg =
            ChatMessage::assistant(content.clone()).with_tool_calls(initial_tool_calls.clone());
        messages.push(assistant_msg);

        // Handle --no-execute mode: record tool calls but don't execute them
        if self.no_execute {
            let actions = build_no_execute_actions(&initial_tool_calls, &mut messages);
            let duration_ms = start_time.elapsed().as_millis();
            return Ok(NonInteractiveResult {
                prompt,
                response: content,
                actions,
                errors,
                metadata: ExecutionMetadata {
                    model: model_name,
                    tokens_used: Some(total_tokens),
                    duration_ms,
                    actions_executed: false,
                },
            });
        }

        // Delegate to shared agent loop for tool execution + model re-calling
        let mut observer = SilentObserver;
        let loop_result = agent_loop::run_agent_loop(
            Arc::clone(&self.model),
            &model_config,
            &mut messages,
            initial_tool_calls,
            &mut observer,
            MAX_AGENT_ITERATIONS,
        )
        .await?;

        // Build result from the agent loop
        total_tokens += loop_result.total_tokens;
        let final_response = if loop_result.final_response.is_empty() {
            content
        } else {
            loop_result.final_response
        };

        let actions: Vec<ActionResult> = loop_result
            .tool_results
            .iter()
            .map(|tr| {
                let (action_type, target) = extract_action_info(&tr.action);
                ActionResult {
                    action_type,
                    target,
                    success: tr.success,
                    output: Some(tr.output.clone()),
                }
            })
            .collect();

        if loop_result.interrupted {
            errors.push("Agent loop was interrupted".to_string());
        }

        let duration_ms = start_time.elapsed().as_millis();
        let actions_executed = !actions.is_empty();
        Ok(NonInteractiveResult {
            prompt,
            response: final_response,
            actions,
            errors,
            metadata: ExecutionMetadata {
                model: model_name,
                tokens_used: Some(total_tokens),
                duration_ms,
                actions_executed,
            },
        })
    }

    /// Format the result according to the output format
    pub fn format_result(&self, result: &NonInteractiveResult, format: OutputFormat) -> String {
        match format {
            OutputFormat::Json => serde_json::to_string_pretty(result).unwrap_or_else(|e| {
                format!("{{\"error\": \"Failed to serialize result: {}\"}}", e)
            }),
            OutputFormat::Text => {
                let mut output = String::new();
                output.push_str(&result.response);

                if !result.actions.is_empty() {
                    output.push_str("\n\n--- Actions ---\n");
                    for action in &result.actions {
                        output.push_str(&format!(
                            "[{}] {} - {}\n",
                            if action.success { "OK" } else { "FAIL" },
                            action.action_type,
                            action.target
                        ));
                        if let Some(ref out) = action.output {
                            output.push_str(&format!("  {}\n", out));
                        }
                    }
                }

                if !result.errors.is_empty() {
                    output.push_str("\n--- Errors ---\n");
                    for error in &result.errors {
                        output.push_str(&format!("• {}\n", error));
                    }
                }

                output
            },
            OutputFormat::Markdown => {
                let mut output = String::new();

                output.push_str("## Response\n\n");
                output.push_str(&result.response);
                output.push_str("\n\n");

                if !result.actions.is_empty() {
                    output.push_str("## Actions Executed\n\n");
                    for action in &result.actions {
                        let status = if action.success { "SUCCESS" } else { "FAILED" };
                        output.push_str(&format!(
                            "- {} **{}**: `{}`\n",
                            status, action.action_type, action.target
                        ));
                        if let Some(ref out) = action.output {
                            output.push_str(&format!("  ```\n  {}\n  ```\n", out));
                        }
                    }
                    output.push('\n');
                }

                if !result.errors.is_empty() {
                    output.push_str("## Errors\n\n");
                    for error in &result.errors {
                        output.push_str(&format!("- {}\n", error));
                    }
                    output.push('\n');
                }

                output.push_str("---\n");
                output.push_str(&format!(
                    "*Model: {} | Tokens: {} | Duration: {}ms*\n",
                    result.metadata.model,
                    result.metadata.tokens_used.unwrap_or(0),
                    result.metadata.duration_ms
                ));

                output
            },
        }
    }
}

/// Extract action type and target description from an AgentAction
fn extract_action_info(action: &AgentAction) -> (String, String) {
    let (label, target) = action.display_info();
    (label.to_lowercase().replace(' ', "_"), target)
}

/// Build ActionResult entries for --no-execute mode (records tool calls without executing)
fn build_no_execute_actions(
    tool_calls: &[crate::models::ToolCall],
    messages: &mut Vec<ChatMessage>,
) -> Vec<ActionResult> {
    let mut actions = Vec::new();
    for tc in tool_calls {
        let tool_call_id = tc.id.clone().unwrap_or_else(|| "call_noexec".to_string());
        let tool_name = tc.function.name.clone();

        let (action_type, target) = match tc.to_agent_action() {
            Ok(action) => extract_action_info(&action),
            Err(_) => (tool_name.clone(), String::new()),
        };

        let msg = "Not executed (--no-execute mode)".to_string();
        messages.push(ChatMessage::tool(&tool_call_id, &tool_name, &msg));
        actions.push(ActionResult {
            action_type,
            target,
            success: false,
            output: Some(msg),
        });
    }
    actions
}

/// Observer that does nothing -- used by non-interactive mode
struct SilentObserver;

impl AgentObserver for SilentObserver {
    fn check_interrupt(&mut self) -> LoopControl {
        LoopControl::Continue
    }
    fn on_status(&mut self, _: &str) {}
    fn on_tool_result(&mut self, _: &str, _: &str, _: &AgentAction, _: &AgentActionResult) {}
    fn on_error(&mut self, _: &str) {}
    fn on_generation_start(&mut self) {}
    fn on_generation_complete(&mut self, _: usize) {}
}
