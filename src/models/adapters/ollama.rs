//! Ollama model adapter
//!
//! Provides unified interface to Ollama (both local and cloud) with connection pooling,
//! health monitoring, and zero-unwrap error handling.

use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

use crate::models::config::{BackendConfig, ModelConfig};
use crate::models::error::{BackendError, ModelError, Result};
use crate::models::traits::Model;
use crate::models::types::{ChatMessage, MessageRole, ModelResponse, StreamCallback, TokenUsage};

/// Mutable accumulators for stream processing, grouped to reduce parameter count.
struct StreamAccumulator {
    content: String,
    thinking: String,
    tool_calls: Vec<crate::models::ToolCall>,
    in_thinking_phase: bool,
    hide_thinking: bool,
    prompt_tokens: usize,
    completion_tokens: usize,
}

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
            .map_err(|e| {
                ModelError::Backend(BackendError::ConnectionFailed {
                    backend: "ollama".to_string(),
                    url: base_url.clone(),
                    reason: e.to_string(),
                })
            })?;

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
        hide_thinking: bool,
    ) -> Result<ModelResponse> {
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let error_text = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(ModelError::Backend(BackendError::HttpError {
                status,
                message: error_text,
            }));
        }

        let mut stream = response.bytes_stream();
        let mut acc = StreamAccumulator {
            content: String::new(),
            thinking: String::new(),
            tool_calls: Vec::new(),
            in_thinking_phase: false,
            hide_thinking,
            prompt_tokens: 0,
            completion_tokens: 0,
        };

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

                let json_chunk: OllamaStreamChunk =
                    serde_json::from_str(&line).map_err(|e| ModelError::ParseError {
                        message: format!("Failed to parse Ollama response: {}", e),
                        raw: Some(line.clone()),
                    })?;

                Self::process_stream_chunk(&json_chunk, &callback, &mut acc);
            }
        }

        // Process any remaining buffered content after the stream ends
        // (the final JSON line may not have a trailing newline)
        if !line_buffer.trim().is_empty() {
            let json_chunk: OllamaStreamChunk =
                serde_json::from_str(line_buffer.trim()).map_err(|e| ModelError::ParseError {
                    message: format!("Failed to parse Ollama response: {}", e),
                    raw: Some(line_buffer.clone()),
                })?;

            Self::process_stream_chunk(&json_chunk, &callback, &mut acc);
        }

        let thinking = if acc.thinking.is_empty() {
            None
        } else {
            Some(acc.thinking)
        };
        let tool_calls = if acc.tool_calls.is_empty() {
            None
        } else {
            Some(acc.tool_calls)
        };

        Ok(ModelResponse {
            content: acc.content,
            usage: Some(TokenUsage {
                prompt_tokens: acc.prompt_tokens,
                completion_tokens: acc.completion_tokens,
                total_tokens: acc.prompt_tokens + acc.completion_tokens,
            }),
            model_name: self.model_name.clone(),
            thinking,
            tool_calls,
        })
    }

    /// Process a single parsed stream chunk, updating all accumulators
    fn process_stream_chunk(
        json_chunk: &OllamaStreamChunk,
        callback: &StreamCallback,
        acc: &mut StreamAccumulator,
    ) {
        // Handle thinking content (if present)
        // When hide_thinking is true, silently discard -- model still thinks
        // server-side but the trace isn't shown to the user (like --hidethinking)
        if let Some(ref thinking_chunk) = json_chunk.message.thinking
            && !acc.hide_thinking
        {
            if !acc.in_thinking_phase {
                callback("Thinking...\n\n");
                acc.in_thinking_phase = true;
            }
            if !thinking_chunk.is_empty() {
                callback(thinking_chunk);
                acc.thinking.push_str(thinking_chunk);
            }
        }

        // Handle tool calls (if present)
        if let Some(ref tool_calls) = json_chunk.message.tool_calls {
            acc.tool_calls.extend(tool_calls.clone());
        }

        // Handle regular content
        if !json_chunk.message.content.is_empty() {
            if acc.in_thinking_phase {
                callback("\n...done thinking.\n\n");
                acc.in_thinking_phase = false;
            }
            callback(&json_chunk.message.content);
            acc.content.push_str(&json_chunk.message.content);
        }

        // Capture token usage
        if json_chunk.done {
            if let Some(count) = json_chunk.prompt_eval_count {
                acc.prompt_tokens = count;
            }
            if let Some(count) = json_chunk.eval_count {
                acc.completion_tokens = count;
            }
        }
    }
}

#[async_trait]
impl Model for OllamaAdapter {
    fn name(&self) -> &str {
        &self.model_name
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        let url = format!("{}/api/tags", self.base_url);

        let response = self.client.get(&url).send().await.map_err(|e| {
            ModelError::Backend(BackendError::ConnectionFailed {
                backend: "ollama".to_string(),
                url: self.base_url.clone(),
                reason: e.to_string(),
            })
        })?;

        if !response.status().is_success() {
            return Err(ModelError::Backend(BackendError::HttpError {
                status: response.status().as_u16(),
                message: "Failed to list models".to_string(),
            }));
        }

        let tags: OllamaTagsResponse =
            response.json().await.map_err(|e| ModelError::ParseError {
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
            if msg.role == MessageRole::Assistant
                && let Some(ref tool_calls) = msg.tool_calls
            {
                json_msg["tool_calls"] = json!(tool_calls);
            }

            // Add tool_name for tool result messages (required by Ollama API)
            // Per Ollama docs: messages.append({'role': 'tool', 'tool_name': tc.function.name, 'content': str(result)})
            if msg.role == MessageRole::Tool
                && let Some(ref tool_name) = msg.tool_name
            {
                json_msg["tool_name"] = json!(tool_name);
            }

            // Add images if present (for multimodal models)
            if let Some(ref images) = msg.images
                && !images.is_empty()
            {
                json_msg["images"] = json!(images);
            }

            json_messages.push(json_msg);
        }

        // Add Ollama native tools for function calling (statically cached)
        let all_tools = crate::models::ToolRegistry::ollama_tools_cached();
        let no_cloud_key = crate::ollama::get_cloud_api_key().is_none();
        let mut tools: Vec<&serde_json::Value> = all_tools
            .iter()
            .filter(|t| {
                let name = t
                    .pointer("/function/name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("");
                // Exclude web tools when no cloud API key is configured
                if no_cloud_key && (name == "web_search" || name == "web_fetch") {
                    return false;
                }
                // Exclude agent tool for subagents (prevents recursive nesting)
                if config.is_subagent && name == "agent" {
                    return false;
                }
                // Exclude computer use tools for subagents — concurrent mouse/keyboard
                // control is inherently broken (shared screen, global coordinates)
                if config.is_subagent
                    && matches!(
                        name,
                        "screenshot"
                            | "list_windows"
                            | "click"
                            | "type_text"
                            | "press_key"
                            | "scroll"
                            | "mouse_move"
                    )
                {
                    return false;
                }
                true
            })
            .collect();

        // Append MCP tools (dynamic, discovered at runtime from MCP servers)
        for mcp_tool in &config.mcp_tools {
            tools.push(mcp_tool);
        }

        // Build request body
        let mut request_body = json!({
            "model": self.model_name,
            "messages": json_messages,
            "stream": stream_callback.is_some(),
            "tools": &tools,
        });

        // Only send think parameter when explicitly set by the user (Alt+T toggle).
        // Omitting it lets the model use its default behavior.
        if let Some(val) = config.thinking_enabled {
            request_body["think"] = json!(val);
        }
        tracing::debug!("think={:?}", config.thinking_enabled);

        tracing::debug!("Sending {} tools to Ollama", tools.len());
        tracing::debug!(
            "Request body tools: {}",
            serde_json::to_string_pretty(&tools).unwrap_or_default()
        );

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
        let response = self
            .client
            .post(&url)
            .json(&request_body)
            .send()
            .await
            .map_err(|e| {
                ModelError::Backend(BackendError::ConnectionFailed {
                    backend: "ollama".to_string(),
                    url: self.base_url.clone(),
                    reason: e.to_string(),
                })
            })?;

        if let Some(callback) = stream_callback {
            let hide_thinking = config.thinking_enabled == Some(false);
            self.handle_stream(response, callback, hide_thinking).await
        } else {
            // Non-streaming response
            if !response.status().is_success() {
                let status = response.status().as_u16();
                let error_text = response
                    .text()
                    .await
                    .unwrap_or_else(|_| "Unknown error".to_string());
                return Err(ModelError::Backend(BackendError::HttpError {
                    status,
                    message: error_text,
                }));
            }

            let json: OllamaStreamChunk =
                response.json().await.map_err(|e| ModelError::ParseError {
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
pub(crate) struct OllamaTagsResponse {
    pub(crate) models: Vec<OllamaModel>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct OllamaModel {
    pub(crate) name: String,
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

    // Add default Ollama port if missing (only for http; https keeps its default 443)
    if let Some(after_scheme) = normalized.strip_prefix("http://")
        && !after_scheme.contains(':')
    {
        normalized = format!("{}:11434", normalized);
    }
    // For https:// without a port, don't add :11434 — the default port (443) is correct

    normalized
}
