use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::utils::MutexExt;

use crate::{
    agents::{ActionResult as AgentActionResult, AgentAction},
    app::Config,
    cli::OutputFormat,
    models::{
        ChatMessage, Model, ModelConfig, ModelFactory, StreamCallback, StreamEvent, ToolCall,
    },
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
    model_config: ModelConfig,
    /// Project instructions auto-loaded from MERMAID.md (Step 5h).
    /// Non-interactive mode is one-shot — load once at construction,
    /// no auto-reload (single execute() call, no per-turn loop).
    instructions: Option<crate::app::instructions::LoadedInstructions>,
}

impl NonInteractiveRunner {
    /// Create a new non-interactive runner
    pub async fn new(
        model_id: String,
        config: Config,
        no_execute: bool,
        max_tokens: Option<usize>,
        reasoning: Option<crate::models::ReasoningLevel>,
    ) -> Result<Self> {
        // Create model instance
        let model = ModelFactory::create(&model_id, Some(&config)).await?;

        // Build base config from app config, then apply CLI overrides
        let mut model_config = ModelConfig::from_app_config(&config, &model_id);
        if let Some(mt) = max_tokens {
            model_config.max_tokens = mt;
        }
        // CLI `--reasoning` wins over the config-file default. Without
        // it, `from_app_config` already populated `reasoning` from
        // `[default_model].reasoning` (Wave 2). Non-interactive mode
        // historically forced thinking off; users now pick that
        // explicitly via `--reasoning none` if that's what they want.
        if let Some(level) = reasoning {
            model_config.reasoning = level;
        }

        // Step 5h: discover MERMAID.md once at construction. The
        // run-once nature of this path means there's no per-turn
        // refresh — you get whatever's on disk when you start.
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let instructions = crate::app::instructions::find_mermaid_md(&cwd)
            .and_then(|p| crate::app::instructions::load_from_path(&p));

        Ok(Self {
            model: Arc::new(RwLock::new(model)),
            no_execute,
            model_config,
            instructions,
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

        // Use pre-built model config; inject the MERMAID.md suffix
        // (Step 5h) loaded at construction. One-shot mode means no
        // mid-run reload — suffix is whatever was on disk at startup.
        let mut effective_config = self.model_config.clone();
        effective_config.dynamic_system_suffix =
            self.instructions.as_ref().map(|i| i.content.clone());
        let model_config = &effective_config;
        let model_name = model_config.model.clone();

        // First model call. Accumulate text + tool calls from typed events;
        // ignore reasoning chunks (`ModelResponse.thinking` is still
        // populated and surfaced via the result if needed).
        let response_text = Arc::new(std::sync::Mutex::new(String::new()));
        let typed_tool_calls = Arc::new(std::sync::Mutex::new(Vec::<ToolCall>::new()));
        let text_clone = Arc::clone(&response_text);
        let tool_clone = Arc::clone(&typed_tool_calls);
        let callback: StreamCallback = Arc::new(move |event| match event {
            StreamEvent::Text(chunk) => {
                text_clone.lock_mut_safe().push_str(&chunk);
            },
            StreamEvent::ToolCall(tc) => {
                tool_clone.lock_mut_safe().push(tc);
            },
            StreamEvent::Reasoning(_) | StreamEvent::Done { .. } => {},
        });

        let result = {
            let model = self.model.read().await;
            model.chat(&messages, model_config, Some(callback)).await
        };

        let (content, initial_tool_calls) = match result {
            Ok(response) => {
                let streamed_text = response_text.lock_mut_safe().clone();
                let content = if !streamed_text.is_empty() {
                    streamed_text
                } else {
                    response.content
                };
                total_tokens += response.usage.map(|u| u.total_tokens).unwrap_or(0);
                let streamed_tool_calls = std::mem::take(&mut *typed_tool_calls.lock_mut_safe());
                let tool_calls = if !streamed_tool_calls.is_empty() {
                    streamed_tool_calls
                } else {
                    response.tool_calls.unwrap_or_default()
                };
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
            model_config,
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
}

/// Format a non-interactive result according to the output format
pub fn format_result(result: &NonInteractiveResult, format: OutputFormat) -> String {
    match format {
        OutputFormat::Json => serde_json::to_string_pretty(result)
            .unwrap_or_else(|e| format!("{{\"error\": \"Failed to serialize result: {}\"}}", e)),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::AgentAction;

    fn sample_result() -> NonInteractiveResult {
        NonInteractiveResult {
            prompt: "Fix the bug".to_string(),
            response: "I fixed the bug.".to_string(),
            actions: vec![ActionResult {
                action_type: "write_file".to_string(),
                target: "src/main.rs".to_string(),
                success: true,
                output: Some("File written".to_string()),
            }],
            errors: vec![],
            metadata: ExecutionMetadata {
                model: "test-model".to_string(),
                tokens_used: Some(100),
                duration_ms: 1234,
                actions_executed: true,
            },
        }
    }

    fn sample_result_with_errors() -> NonInteractiveResult {
        NonInteractiveResult {
            prompt: "Do something".to_string(),
            response: "Tried but failed.".to_string(),
            actions: vec![ActionResult {
                action_type: "bash".to_string(),
                target: "cargo test".to_string(),
                success: false,
                output: Some("tests failed".to_string()),
            }],
            errors: vec!["Command failed".to_string()],
            metadata: ExecutionMetadata {
                model: "test-model".to_string(),
                tokens_used: Some(50),
                duration_ms: 500,
                actions_executed: true,
            },
        }
    }

    #[test]
    fn test_extract_action_info_read() {
        let action = AgentAction::ReadFile {
            paths: vec!["foo.rs".to_string()],
        };
        let (action_type, target) = extract_action_info(&action);
        assert_eq!(action_type, "read");
        assert_eq!(target, "foo.rs");
    }

    #[test]
    fn test_extract_action_info_bash() {
        let action = AgentAction::ExecuteCommand {
            command: "cargo test".to_string(),
            working_dir: None,
            timeout: None,
        };
        let (action_type, target) = extract_action_info(&action);
        assert_eq!(action_type, "bash");
        assert_eq!(target, "cargo test");
    }

    #[test]
    fn test_extract_action_info_web_search() {
        let action = AgentAction::WebSearch {
            queries: vec![("rust async".to_string(), 5)],
        };
        let (action_type, target) = extract_action_info(&action);
        assert_eq!(action_type, "web_search");
        assert_eq!(target, "rust async");
    }

    #[test]
    fn test_extract_action_info_write() {
        let action = AgentAction::WriteFile {
            path: "out.txt".to_string(),
            content: "hello".to_string(),
        };
        let (action_type, target) = extract_action_info(&action);
        assert_eq!(action_type, "write");
        assert_eq!(target, "out.txt");
    }

    #[test]
    fn test_format_result_json() {
        let result = sample_result();
        let json = format_result(&result, OutputFormat::Json);
        assert!(json.contains("\"prompt\": \"Fix the bug\""));
        assert!(json.contains("\"success\": true"));
        assert!(json.contains("\"model\": \"test-model\""));
    }

    #[test]
    fn test_format_result_text() {
        let result = sample_result();
        let text = format_result(&result, OutputFormat::Text);
        assert!(text.contains("I fixed the bug."));
        assert!(text.contains("[OK] write_file - src/main.rs"));
        assert!(text.contains("--- Actions ---"));
    }

    #[test]
    fn test_format_result_text_with_errors() {
        let result = sample_result_with_errors();
        let text = format_result(&result, OutputFormat::Text);
        assert!(text.contains("[FAIL] bash - cargo test"));
        assert!(text.contains("--- Errors ---"));
        assert!(text.contains("Command failed"));
    }

    #[test]
    fn test_format_result_markdown() {
        let result = sample_result();
        let md = format_result(&result, OutputFormat::Markdown);
        assert!(md.contains("## Response"));
        assert!(md.contains("I fixed the bug."));
        assert!(md.contains("## Actions Executed"));
        assert!(md.contains("SUCCESS **write_file**"));
        assert!(md.contains("*Model: test-model"));
    }

    #[test]
    fn test_format_result_text_no_actions() {
        let result = NonInteractiveResult {
            prompt: "hi".to_string(),
            response: "hello".to_string(),
            actions: vec![],
            errors: vec![],
            metadata: ExecutionMetadata {
                model: "m".to_string(),
                tokens_used: None,
                duration_ms: 10,
                actions_executed: false,
            },
        };
        let text = format_result(&result, OutputFormat::Text);
        assert_eq!(text, "hello");
        assert!(!text.contains("Actions"));
    }
}
