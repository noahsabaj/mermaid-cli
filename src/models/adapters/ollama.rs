/// Ollama model adapter
///
/// Provides unified interface to Ollama (both local and cloud) with connection pooling,
/// health monitoring, and zero-unwrap error handling.

use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

use crate::models::config::{BackendConfig, ModelConfig};
use crate::models::error::{BackendError, ModelError, Result};
use crate::models::traits::{Model, ModelCapabilities};
use crate::models::types::{ChatMessage, MessageRole, ModelResponse, ProjectContext, StreamCallback, TokenUsage};
use crate::utils::{retry_async, RetryConfig};

/// Ollama model adapter
pub struct OllamaAdapter {
    client: Client,
    base_url: String,
    model_name: String,
}

impl OllamaAdapter {
    /// Create a new Ollama adapter for a specific model
    pub async fn new(model_name: &str, config: Arc<BackendConfig>) -> Result<Self> {
        let base_url = normalize_url(&config.ollama_url);

        // Build HTTP client with connection pooling
        let client = Client::builder()
            .pool_max_idle_per_host(config.max_idle_per_host)
            .pool_idle_timeout(Duration::from_secs(90))
            .timeout(Duration::from_secs(config.request_timeout_secs))
            .tcp_keepalive(Duration::from_secs(60))
            .build()
            .map_err(|e| ModelError::Backend(BackendError::ConnectionFailed {
                backend: "ollama".to_string(),
                url: base_url.clone(),
                reason: e.to_string(),
            }))?;

        Ok(Self {
            client,
            base_url,
            model_name: model_name.to_string(),
        })
    }

    /// Handle streaming response
    async fn handle_stream(
        &self,
        response: reqwest::Response,
        callback: StreamCallback,
    ) -> Result<ModelResponse> {
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let error_text = response.text().await.unwrap_or_else(|_| "Unknown error".to_string());
            return Err(ModelError::Backend(BackendError::HttpError {
                status,
                message: error_text,
            }));
        }

        let mut stream = response.bytes_stream();
        let mut full_content = String::new();
        let mut full_thinking = String::new();
        let mut accumulated_tool_calls: Vec<crate::models::ToolCall> = Vec::new();
        let mut in_thinking_phase = false;
        let mut prompt_tokens = 0;
        let mut completion_tokens = 0;

        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result.map_err(|e| ModelError::StreamError(e.to_string()))?;

            let text = String::from_utf8_lossy(&chunk);

            // Parse each line as JSON
            for line in text.lines() {
                if line.trim().is_empty() {
                    continue;
                }

                let json_chunk: OllamaStreamChunk = serde_json::from_str(line)
                    .map_err(|e| ModelError::ParseError {
                        message: format!("Failed to parse Ollama response: {}", e),
                        raw: Some(line.to_string()),
                    })?;

                // Handle thinking content (if present)
                if let Some(ref thinking_chunk) = json_chunk.message.thinking {
                    if !in_thinking_phase {
                        callback("Thinking...\n\n");
                        in_thinking_phase = true;
                    }
                    if !thinking_chunk.is_empty() {
                        callback(thinking_chunk);
                        full_thinking.push_str(thinking_chunk);
                    }
                }

                // Handle tool calls (if present)
                if let Some(tool_calls) = json_chunk.message.tool_calls {
                    accumulated_tool_calls.extend(tool_calls.clone());
                    // Send tool calls to stream handler as special marker
                    if let Ok(tool_calls_json) = serde_json::to_string(&tool_calls) {
                        callback(&format!("[TOOL_CALLS:{}]", tool_calls_json));
                    }
                }

                // Handle regular content
                if !json_chunk.message.content.is_empty() {
                    // Transition from thinking to answer
                    if in_thinking_phase {
                        callback("\n...done thinking.\n\n");
                        in_thinking_phase = false;
                    }
                    callback(&json_chunk.message.content);
                    full_content.push_str(&json_chunk.message.content);
                }

                // Capture token usage
                if json_chunk.done {
                    if let Some(count) = json_chunk.prompt_eval_count {
                        prompt_tokens = count;
                    }
                    if let Some(count) = json_chunk.eval_count {
                        completion_tokens = count;
                    }
                }
            }
        }

        let thinking = if full_thinking.is_empty() {
            None
        } else {
            Some(full_thinking)
        };

        let tool_calls = if accumulated_tool_calls.is_empty() {
            None
        } else {
            Some(accumulated_tool_calls)
        };

        Ok(ModelResponse {
            content: full_content,
            usage: Some(TokenUsage {
                prompt_tokens,
                completion_tokens,
                total_tokens: prompt_tokens + completion_tokens,
            }),
            model_name: self.model_name.clone(),
            thinking,
            tool_calls,
        })
    }
}

#[async_trait]
impl Model for OllamaAdapter {
    fn name(&self) -> &str {
        &self.model_name
    }

    fn is_local(&self) -> bool {
        // Ollama daemon is local, even if it routes to cloud
        true
    }

    async fn health_check(&self) -> Result<()> {
        let url = format!("{}/api/tags", self.base_url);
        let base_url = self.base_url.clone();

        // Retry config for health checks (3 attempts with quick backoff)
        let retry_config = RetryConfig {
            max_attempts: 3,
            initial_delay_ms: 500,
            max_delay_ms: 3000,
            backoff_multiplier: 2.0,
        };

        let client = self.client.clone();
        let health_result = retry_async(
            || {
                let client = client.clone();
                let url = url.clone();
                async move {
                    let response = client
                        .get(&url)
                        .send()
                        .await
                        .map_err(|e| anyhow::anyhow!("Health check failed: {}", e))?;

                    if response.status().is_success() {
                        Ok(())
                    } else {
                        Err(anyhow::anyhow!("HTTP {}", response.status()))
                    }
                }
            },
            &retry_config,
        )
        .await;

        health_result.map_err(|e| ModelError::Backend(BackendError::ConnectionFailed {
            backend: "ollama".to_string(),
            url: base_url,
            reason: e.to_string(),
        }))
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        let url = format!("{}/api/tags", self.base_url);

        let response = self.client
            .get(&url)
            .send()
            .await
            .map_err(|e| ModelError::Backend(BackendError::ConnectionFailed {
                backend: "ollama".to_string(),
                url: self.base_url.clone(),
                reason: e.to_string(),
            }))?;

        if !response.status().is_success() {
            return Err(ModelError::Backend(BackendError::HttpError {
                status: response.status().as_u16(),
                message: "Failed to list models".to_string(),
            }));
        }

        let tags: OllamaTagsResponse = response.json().await
            .map_err(|e| ModelError::ParseError {
                message: format!("Failed to parse tags response: {}", e),
                raw: None,
            })?;

        Ok(tags.models.into_iter().map(|m| m.name).collect())
    }

    async fn chat(
        &self,
        messages: &[ChatMessage],
        context: &ProjectContext,
        config: &ModelConfig,
        stream_callback: Option<StreamCallback>,
    ) -> Result<ModelResponse> {
        let url = format!("{}/api/chat", self.base_url);

        // Extract Ollama-specific options
        let ollama_opts = config.ollama_options();

        // Build messages array
        let mut json_messages = Vec::new();

        // Add system prompt if configured
        if let Some(ref system_prompt) = config.system_prompt {
            json_messages.push(json!({
                "role": "system",
                "content": system_prompt
            }));
        }

        // Add project context
        let context_str = context.to_prompt_context();
        if !context_str.is_empty() {
            json_messages.push(json!({
                "role": "system",
                "content": format!("Project Context:\n{}", context_str)
            }));
        }

        // Add conversation messages
        for msg in messages {
            let role = match msg.role {
                MessageRole::User => "user",
                MessageRole::Assistant => "assistant",
                MessageRole::System => "system",
                MessageRole::Tool => "tool",
            };

            let mut json_msg = json!({
                "role": role,
                "content": msg.content
            });

            // Add tool_call_id and tool_name for tool result messages (required by Ollama API)
            if msg.role == MessageRole::Tool {
                if let Some(ref tool_call_id) = msg.tool_call_id {
                    json_msg["tool_call_id"] = json!(tool_call_id);
                }
                if let Some(ref tool_name) = msg.tool_name {
                    json_msg["name"] = json!(tool_name);
                }
            }

            // Add images if present (for multimodal models)
            if let Some(ref images) = msg.images {
                if !images.is_empty() {
                    json_msg["images"] = json!(images);
                }
            }

            json_messages.push(json_msg);
        }

        // Add Ollama native tools for function calling
        let tool_registry = crate::models::ToolRegistry::mermaid_tools();
        let tools = tool_registry.to_ollama_format();

        // Build request body
        let mut request_body = json!({
            "model": self.model_name,
            "messages": json_messages,
            "stream": stream_callback.is_some(),
            "tools": tools,
        });

        // Add model parameters
        let mut options = json!({});
        if let Some(temp) = Some(config.temperature) {
            options["temperature"] = json!(temp);
        }
        if let Some(num_ctx) = ollama_opts.num_ctx {
            options["num_ctx"] = json!(num_ctx);
        }
        if let Some(num_gpu) = ollama_opts.num_gpu {
            options["num_gpu"] = json!(num_gpu);
        }
        if let Some(num_thread) = ollama_opts.num_thread {
            options["num_thread"] = json!(num_thread);
        }
        if let Some(numa) = ollama_opts.numa {
            options["numa"] = json!(numa);
        }

        if !options.as_object().map(|o| o.is_empty()).unwrap_or(true) {
            request_body["options"] = options;
        }

        // Send request
        let response = self.client
            .post(&url)
            .json(&request_body)
            .send()
            .await
            .map_err(|e| ModelError::Backend(BackendError::ConnectionFailed {
                backend: "ollama".to_string(),
                url: self.base_url.clone(),
                reason: e.to_string(),
            }))?;

        if let Some(callback) = stream_callback {
            self.handle_stream(response, callback).await
        } else {
            // Non-streaming response
            if !response.status().is_success() {
                let status = response.status().as_u16();
                let error_text = response.text().await.unwrap_or_else(|_| "Unknown error".to_string());
                return Err(ModelError::Backend(BackendError::HttpError {
                    status,
                    message: error_text,
                }));
            }

            let json: OllamaStreamChunk = response.json().await
                .map_err(|e| ModelError::ParseError {
                    message: format!("Failed to parse response: {}", e),
                    raw: None,
                })?;

            let thinking = json.message.thinking.filter(|t| !t.is_empty());
            let tool_calls = json.message.tool_calls.filter(|tc| !tc.is_empty());

            Ok(ModelResponse {
                content: json.message.content,
                usage: None,
                model_name: self.model_name.clone(),
                thinking,
                tool_calls,
            })
        }
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            max_context_length: 8192,
            supports_streaming: true,
            supports_functions: true,
            supports_vision: true,
        }
    }
}

// Response types

#[derive(Debug, Serialize, Deserialize)]
struct OllamaStreamChunk {
    message: OllamaMessage,
    done: bool,
    #[serde(default)]
    prompt_eval_count: Option<usize>,
    #[serde(default)]
    eval_count: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize)]
struct OllamaMessage {
    role: String,
    content: String,
    #[serde(default)]
    thinking: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<crate::models::ToolCall>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct OllamaTagsResponse {
    models: Vec<OllamaModel>,
}

#[derive(Debug, Serialize, Deserialize)]
struct OllamaModel {
    name: String,
}

// Helper functions

fn normalize_url(url: &str) -> String {
    let mut normalized = url.trim().to_string();

    // Replace 0.0.0.0 with 127.0.0.1
    if normalized.contains("0.0.0.0") {
        normalized = normalized.replace("0.0.0.0", "127.0.0.1");
    }

    // Add http:// if missing
    if !normalized.starts_with("http://") && !normalized.starts_with("https://") {
        normalized = format!("http://{}", normalized);
    }

    // Add default port if missing
    if !normalized.contains(':') || normalized.matches(':').count() == 1 {
        if normalized.starts_with("http://") && !normalized[7..].contains(':') {
            normalized = format!("{}:11434", normalized);
        } else if normalized.starts_with("https://") && !normalized[8..].contains(':') {
            normalized = format!("{}:11434", normalized);
        }
    }

    normalized
}
