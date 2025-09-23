use anyhow::Result;
use colored::Colorize;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::{
    app::{Config},
    cli::OutputFormat,
    context::ContextLoader,
    models::{ChatMessage, MessageRole, Model, ModelConfig, ModelFactory, ProjectContext},
    agents::{execute_action, parse_actions, AgentAction, ActionResult as AgentActionResult},
};

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
    model: Arc<Mutex<Box<dyn Model>>>,
    context: ProjectContext,
    config: Config,
    no_execute: bool,
    max_tokens: Option<usize>,
}

impl NonInteractiveRunner {
    /// Create a new non-interactive runner
    pub async fn new(
        model_id: String,
        project_path: PathBuf,
        config: Config,
        no_execute: bool,
        max_tokens: Option<usize>,
    ) -> Result<Self> {
        // Create model instance
        let model = ModelFactory::create(&model_id, Some(&config)).await?;

        // Load project context
        let loader = ContextLoader::new()?;
        let context = loader.load_context(&project_path)?;

        Ok(Self {
            model: Arc::new(Mutex::new(model)),
            context,
            config,
            no_execute,
            max_tokens,
        })
    }

    /// Execute a single prompt and return the result
    pub async fn execute(&self, prompt: String) -> Result<NonInteractiveResult> {
        let start_time = std::time::Instant::now();
        let mut errors = Vec::new();
        let mut actions = Vec::new();

        // Build messages - only include project context if the prompt seems code-related
        let prompt_lower = prompt.to_lowercase();
        let is_code_related = prompt_lower.contains("code")
            || prompt_lower.contains("file")
            || prompt_lower.contains("function")
            || prompt_lower.contains("class")
            || prompt_lower.contains("implement")
            || prompt_lower.contains("create")
            || prompt_lower.contains("write")
            || prompt_lower.contains("debug")
            || prompt_lower.contains("fix")
            || prompt_lower.contains("test")
            || prompt_lower.contains("build")
            || prompt_lower.contains("project")
            || prompt_lower.contains("analyze")
            || prompt_lower.contains("refactor");

        let system_content = if is_code_related {
            format!(
                "You are an AI coding assistant. Here is the project structure:\n\n{}\n\n{}",
                self.context.to_prompt_context(),
                "You can use [FILE_WRITE: path] ... [/FILE_WRITE] blocks to write files, and [COMMAND: cmd] ... [/COMMAND] blocks to run commands."
            )
        } else {
            "You are a helpful AI assistant. Answer the user's question directly and concisely.".to_string()
        };

        let system_message = ChatMessage {
            role: MessageRole::System,
            content: system_content,
            timestamp: chrono::Local::now(),
        };

        let user_message = ChatMessage {
            role: MessageRole::User,
            content: prompt.clone(),
            timestamp: chrono::Local::now(),
        };

        let messages = vec![system_message, user_message];

        // Create model config
        let model_config = ModelConfig {
            temperature: Some(0.7),
            max_tokens: self.max_tokens.or(Some(4096)),
            top_p: Some(1.0),
            frequency_penalty: None,
            presence_penalty: None,
            system_prompt: None,
        };

        // Send prompt to model
        let mut full_response = String::new();
        let tokens_used;

        // Create a callback to capture the response
        let response_text = Arc::new(std::sync::Mutex::new(String::new()));
        let response_clone = Arc::clone(&response_text);
        let callback = Arc::new(move |chunk: &str| {
            let mut resp = response_clone.lock().unwrap();
            resp.push_str(chunk);
        });

        // Call the model
        let model_name;
        let result = {
            let mut model = self.model.lock().await;
            model_name = model.name().to_string();
            model.chat(&messages, &self.context, &model_config, Some(callback)).await
        };

        match result {
            Ok(response) => {
                // Try to get content from the callback first
                let callback_content = response_text.lock().unwrap().clone();
                if !callback_content.is_empty() {
                    full_response = callback_content;
                } else {
                    full_response = response.content;
                }
                tokens_used = response.usage.map(|u| u.total_tokens).unwrap_or(0);
            }
            Err(e) => {
                errors.push(format!("Model error: {}", e));
                full_response = response_text.lock().unwrap().clone();
                tokens_used = 0;
            }
        }

        // Parse actions from response
        let parsed_actions = parse_actions(&full_response);

        // Execute actions if not in no-execute mode
        if !self.no_execute && !parsed_actions.is_empty() {
            for action in parsed_actions {
                let (action_type, target) = match &action {
                    AgentAction::WriteFile { path, .. } => ("file_write", path.clone()),
                    AgentAction::ExecuteCommand { command, .. } => ("command", command.clone()),
                    AgentAction::ReadFile { path } => ("file_read", path.clone()),
                    AgentAction::CreateDirectory { path } => ("create_dir", path.clone()),
                    AgentAction::DeleteFile { path } => ("delete_file", path.clone()),
                    AgentAction::GitDiff { .. } => ("git_diff", "git diff".to_string()),
                    AgentAction::GitStatus => ("git_status", "git status".to_string()),
                    AgentAction::GitCommit { message, .. } => ("git_commit", message.clone()),
                };

                let result = execute_action(&action).await.unwrap_or(AgentActionResult::Error {
                    error: "Failed to execute action".to_string(),
                });

                let action_result = match result {
                    AgentActionResult::Success { output } => ActionResult {
                        action_type: action_type.to_string(),
                        target,
                        success: true,
                        output: Some(output),
                    },
                    AgentActionResult::Error { error } => ActionResult {
                        action_type: action_type.to_string(),
                        target,
                        success: false,
                        output: Some(error),
                    },
                };

                actions.push(action_result);
            }
        } else if !parsed_actions.is_empty() {
            // Actions were found but not executed (no-execute mode)
            for action in parsed_actions {
                let (action_type, target) = match action {
                    AgentAction::WriteFile { path, .. } => ("file_write", path),
                    AgentAction::ExecuteCommand { command, .. } => ("command", command),
                    AgentAction::ReadFile { path } => ("file_read", path),
                    AgentAction::CreateDirectory { path } => ("create_dir", path),
                    AgentAction::DeleteFile { path } => ("delete_file", path),
                    AgentAction::GitDiff { .. } => ("git_diff", "git diff".to_string()),
                    AgentAction::GitStatus => ("git_status", "git status".to_string()),
                    AgentAction::GitCommit { message, .. } => ("git_commit", message),
                };

                actions.push(ActionResult {
                    action_type: action_type.to_string(),
                    target,
                    success: false,
                    output: Some("Not executed (--no-execute mode)".to_string()),
                });
            }
        }

        let duration_ms = start_time.elapsed().as_millis();
        let actions_executed = !self.no_execute && !actions.is_empty();

        Ok(NonInteractiveResult {
            prompt,
            response: full_response,
            actions,
            errors,
            metadata: ExecutionMetadata {
                model: model_name,
                tokens_used: Some(tokens_used),
                duration_ms,
                actions_executed,
            },
        })
    }

    /// Format the result according to the output format
    pub fn format_result(&self, result: &NonInteractiveResult, format: OutputFormat) -> String {
        match format {
            OutputFormat::Json => {
                serde_json::to_string_pretty(result).unwrap_or_else(|e| {
                    format!("{{\"error\": \"Failed to serialize result: {}\"}}", e)
                })
            }
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
            }
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
                            status,
                            action.action_type,
                            action.target
                        ));
                        if let Some(ref out) = action.output {
                            output.push_str(&format!("  ```\n  {}\n  ```\n", out));
                        }
                    }
                    output.push_str("\n");
                }

                if !result.errors.is_empty() {
                    output.push_str("## Errors\n\n");
                    for error in &result.errors {
                        output.push_str(&format!("- {}\n", error));
                    }
                    output.push_str("\n");
                }

                output.push_str("---\n");
                output.push_str(&format!(
                    "*Model: {} | Tokens: {} | Duration: {}ms*\n",
                    result.metadata.model,
                    result.metadata.tokens_used.unwrap_or(0),
                    result.metadata.duration_ms
                ));

                output
            }
        }
    }
}