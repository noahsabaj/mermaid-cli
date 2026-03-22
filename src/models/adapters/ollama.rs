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
use crate::models::types::{ChatMessage, MessageRole, ModelResponse, StreamCallback, TokenUsage};
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
        // No global timeout -- streaming responses from cloud models can take
        // minutes for large contexts. Per-request timeouts are set where needed.
        let client = Client::builder()
            .pool_max_idle_per_host(config.max_idle_per_host)
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(60))
            .connect_timeout(Duration::from_secs(config.timeout_secs))
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

        // Buffer for incomplete JSON lines split across TCP chunks.
        // Ollama sends newline-delimited JSON, but bytes_stream() chunks
        // don't align with line boundaries -- a JSON object can be split
        // across two or more TCP packets.
        let mut line_buffer = String::new();

        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result.map_err(|e| ModelError::StreamError(e.to_string()))?;

            let text = String::from_utf8_lossy(&chunk);
            line_buffer.push_str(&text);

            // Process only complete lines (terminated by newline)
            while let Some(newline_pos) = line_buffer.find('\n') {
                let line: String = line_buffer[..newline_pos].to_string();
                line_buffer = line_buffer[newline_pos + 1..].to_string();

                if line.trim().is_empty() {
                    continue;
                }

                let json_chunk: OllamaStreamChunk = serde_json::from_str(&line)
                    .map_err(|e| ModelError::ParseError {
                        message: format!("Failed to parse Ollama response: {}", e),
                        raw: Some(line.clone()),
                    })?;

                self.process_stream_chunk(
                    &json_chunk,
                    &callback,
                    &mut full_content,
                    &mut full_thinking,
                    &mut accumulated_tool_calls,
                    &mut in_thinking_phase,
                    &mut prompt_tokens,
                    &mut completion_tokens,
                );
            }
        }

        // Process any remaining buffered content after the stream ends
        // (the final JSON line may not have a trailing newline)
        if !line_buffer.trim().is_empty() {
            let json_chunk: OllamaStreamChunk = serde_json::from_str(line_buffer.trim())
                .map_err(|e| ModelError::ParseError {
                    message: format!("Failed to parse Ollama response: {}", e),
                    raw: Some(line_buffer.clone()),
                })?;

            self.process_stream_chunk(
                &json_chunk,
                &callback,
                &mut full_content,
                &mut full_thinking,
                &mut accumulated_tool_calls,
                &mut in_thinking_phase,
                &mut prompt_tokens,
                &mut completion_tokens,
            );
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

    /// Process a single parsed stream chunk, updating all accumulators
    fn process_stream_chunk(
        &self,
        json_chunk: &OllamaStreamChunk,
        callback: &StreamCallback,
        full_content: &mut String,
        full_thinking: &mut String,
        accumulated_tool_calls: &mut Vec<crate::models::ToolCall>,
        in_thinking_phase: &mut bool,
        prompt_tokens: &mut usize,
        completion_tokens: &mut usize,
    ) {
        // Handle thinking content (if present)
        if let Some(ref thinking_chunk) = json_chunk.message.thinking {
            if !*in_thinking_phase {
                callback("Thinking...\n\n");
                *in_thinking_phase = true;
            }
            if !thinking_chunk.is_empty() {
                callback(thinking_chunk);
                full_thinking.push_str(thinking_chunk);
            }
        }

        // Handle tool calls (if present)
        if let Some(ref tool_calls) = json_chunk.message.tool_calls {
            accumulated_tool_calls.extend(tool_calls.clone());
            if let Ok(tool_calls_json) = serde_json::to_string(&tool_calls) {
                callback(&format!("[TOOL_CALLS:{}]", tool_calls_json));
            }
        }

        // Handle regular content
        if !json_chunk.message.content.is_empty() {
            if *in_thinking_phase {
                callback("\n...done thinking.\n\n");
                *in_thinking_phase = false;
            }
            callback(&json_chunk.message.content);
            full_content.push_str(&json_chunk.message.content);
        }

        // Capture token usage
        if json_chunk.done {
            if let Some(count) = json_chunk.prompt_eval_count {
                *prompt_tokens = count;
            }
            if let Some(count) = json_chunk.eval_count {
                *completion_tokens = count;
            }
        }
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

        // Add conversation messages (LLM explores codebase via tools, no context injection)
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

            // Add tool_calls for assistant messages (required for agent loop)
            // The assistant message must include the tool_calls it made
            if msg.role == MessageRole::Assistant {
                if let Some(ref tool_calls) = msg.tool_calls {
                    json_msg["tool_calls"] = json!(tool_calls);
                }
            }

            // Add tool_name for tool result messages (required by Ollama API)
            // Per Ollama docs: messages.append({'role': 'tool', 'tool_name': tc.function.name, 'content': str(result)})
            if msg.role == MessageRole::Tool {
                if let Some(ref tool_name) = msg.tool_name {
                    json_msg["tool_name"] = json!(tool_name);
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

        // Add Ollama native tools for function calling (statically cached)
        let tools = crate::models::ToolRegistry::ollama_tools_cached();

        // Build request body
        let mut request_body = json!({
            "model": self.model_name,
            "messages": json_messages,
            "stream": stream_callback.is_some(),
            "tools": tools,
        });

        // Explicitly set think parameter so models that default to thinking
        // (e.g., kimi-k2.5, qwen3) respect the user's toggle (Alt+T)
        request_body["think"] = json!(config.thinking_enabled);

        tracing::debug!("Sending {} tools to Ollama", tools.len());
        tracing::debug!("Request body tools: {}", serde_json::to_string_pretty(&tools).unwrap_or_default());

        // Add model parameters
        let mut options = json!({});
        options["temperature"] = json!(config.temperature);
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

        request_body["options"] = options;

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
    // Strip the scheme prefix to check if the host:port portion already contains a port
    if let Some(after_scheme) = normalized.strip_prefix("http://") {
        if !after_scheme.contains(':') {
            normalized = format!("{}:11434", normalized);
        }
    } else if let Some(after_scheme) = normalized.strip_prefix("https://") {
        if !after_scheme.contains(':') {
            normalized = format!("{}:11434", normalized);
        }
    }

    normalized
}
