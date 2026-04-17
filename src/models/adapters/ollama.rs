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

use crate::constants::MAX_RESPONSE_CHARS;
use crate::models::ModelCapabilities;
use crate::models::config::{BackendConfig, ModelConfig};
use crate::models::error::{BackendError, ModelError, Result};
use crate::models::reasoning::{ReasoningChunk, ReasoningLevel};
use crate::models::stream::{StreamCallback, StreamEvent};
use crate::models::traits::Model;
use crate::models::types::{ChatMessage, MessageRole, ModelResponse, TokenUsage};
use crate::utils::drain_complete_lines;

/// Marker appended to `content` and `thinking` once the per-stream size cap
/// is hit. Subsequent chunks are silently dropped so a runaway model can't
/// exhaust memory in non-interactive mode or sub-agents (the TUI applies its
/// own cap on the buffered response, but the adapter is the only line of
/// defense for non-TUI callers).
const TRUNCATION_MARKER: &str = "\n\n[TRUNCATED: response exceeded size limit]";

/// Mutable accumulators for stream processing, grouped to reduce parameter count.
struct StreamAccumulator {
    content: String,
    thinking: String,
    tool_calls: Vec<crate::models::ToolCall>,
    /// Suppress `StreamEvent::Reasoning` emission to the typed callback.
    /// The accumulator still records `thinking` so `ModelResponse.thinking`
    /// stays populated for backward-compat callers — only the user-visible
    /// stream is gated. Mirrors Ollama's `--hidethinking` semantics.
    hide_reasoning_trace: bool,
    prompt_tokens: usize,
    completion_tokens: usize,
    /// True once `content` OR `thinking` has been truncated to the size cap.
    /// Once set, further chunks are silently dropped — both from the
    /// accumulator AND from typed-event emission. Prevents a runaway model
    /// from filling memory in non-interactive mode and sub-agents.
    truncated: bool,
}

/// Append `chunk` to `buf`, char-boundary-safe truncation at `cap` bytes.
/// Sets `*truncated` once tripped; subsequent calls become no-ops.
fn push_capped(buf: &mut String, chunk: &str, truncated: &mut bool, cap: usize) {
    if *truncated {
        return;
    }
    buf.push_str(chunk);
    if buf.len() > cap {
        let end = buf.floor_char_boundary(cap);
        buf.truncate(end);
        buf.push_str(TRUNCATION_MARKER);
        *truncated = true;
    }
}

/// Ollama model adapter
pub struct OllamaAdapter {
    client: Client,
    base_url: String,
    model_name: String,
    capabilities: ModelCapabilities,
}

/// True if the given model name is a gpt-oss variant. Matching is
/// case-insensitive and prefix-based so tagged variants (`gpt-oss:20b`,
/// `gpt-oss:120b-cloud`) and unknown future sizes all route correctly.
fn is_gpt_oss(model_name: &str) -> bool {
    model_name.to_lowercase().starts_with("gpt-oss")
}

/// Render the `think` field for an Ollama request.
///
/// Ollama accepts two incompatible shapes for this field:
/// - Most models (qwen3, deepseek-r1, kimi-k2-thinking, ...) take `think: bool`.
/// - **gpt-oss** models take `think: "low"|"medium"|"high"` (string enum).
///
/// Sending a bool to gpt-oss silently uses the default effort; sending a
/// string to non-gpt-oss models 400s. This dispatch picks the right shape
/// by inspecting the model name.
fn think_for_ollama(model_name: &str, level: ReasoningLevel) -> serde_json::Value {
    if is_gpt_oss(model_name) {
        let effort = match level {
            // gpt-oss can't truly disable thinking. `None` collapses to
            // `"low"` (the closest-to-off tier) rather than silently
            // upgrading the user's explicit choice to `"medium"`.
            ReasoningLevel::None | ReasoningLevel::Minimal | ReasoningLevel::Low => "low",
            ReasoningLevel::Medium => "medium",
            ReasoningLevel::High | ReasoningLevel::Max | ReasoningLevel::XHigh => "high",
        };
        serde_json::Value::String(effort.to_string())
    } else {
        serde_json::Value::Bool(level != ReasoningLevel::None)
    }
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

        // gpt-oss exposes a discrete `low|medium|high` enum rather than
        // Ollama's usual binary `think: bool`. Advertising `Levels` here
        // routes `ReasoningLevel::XHigh` / `Max` through `nearest_effort`
        // → `High`, which `think_for_ollama` then renders as `"high"`.
        let capabilities = if is_gpt_oss(model_name) {
            ModelCapabilities {
                supports_tools: true,
                supports_vision: false,
                supports_reasoning: crate::models::ReasoningCapability::Levels(vec![
                    ReasoningLevel::None,
                    ReasoningLevel::Low,
                    ReasoningLevel::Medium,
                    ReasoningLevel::High,
                ]),
                max_context_tokens: None,
            }
        } else {
            ModelCapabilities::ollama_default()
        };

        Ok(Self {
            client,
            base_url,
            model_name: model_name.to_string(),
            capabilities,
        })
    }

    /// Handle a streaming response, emitting typed `StreamEvent`s through
    /// the optional callback. The legacy text-callback shape is provided
    /// by the `chat()` shim below, which translates these typed events
    /// back into the marker-encoded text stream the older callers expect.
    async fn handle_stream(
        &self,
        response: reqwest::Response,
        callback: Option<StreamCallback>,
        hide_reasoning_trace: bool,
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
            hide_reasoning_trace,
            prompt_tokens: 0,
            completion_tokens: 0,
            truncated: false,
        };

        // Buffer for incomplete JSON lines split across TCP chunks. Ollama
        // sends newline-delimited JSON, but bytes_stream() chunks don't
        // align with line boundaries — a JSON object can split across two
        // or more TCP packets. We also buffer raw bytes (Vec<u8>) rather
        // than a String because TCP chunks don't align with UTF-8
        // codepoint boundaries either; see `drain_complete_lines` for the
        // full rationale.
        let mut line_buffer: Vec<u8> = Vec::new();

        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result.map_err(|e| ModelError::StreamError(e.to_string()))?;
            line_buffer.extend_from_slice(&chunk);

            for line in drain_complete_lines(&mut line_buffer) {
                if line.trim().is_empty() {
                    continue;
                }

                let json_chunk: OllamaStreamChunk =
                    serde_json::from_str(&line).map_err(|e| ModelError::ParseError {
                        message: format!("Failed to parse Ollama response: {}", e),
                        raw: Some(line.clone()),
                    })?;

                Self::process_stream_chunk(&json_chunk, callback.as_ref(), &mut acc);
            }
        }

        // Process any remaining buffered content after the stream ends
        // (the final JSON line may not have a trailing newline).
        if !line_buffer.is_empty() {
            let trailing = String::from_utf8_lossy(&line_buffer).into_owned();
            if !trailing.trim().is_empty() {
                let json_chunk: OllamaStreamChunk =
                    serde_json::from_str(trailing.trim()).map_err(|e| ModelError::ParseError {
                        message: format!("Failed to parse Ollama response: {}", e),
                        raw: Some(trailing.clone()),
                    })?;

                Self::process_stream_chunk(&json_chunk, callback.as_ref(), &mut acc);
            }
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
        let total_tokens = acc.prompt_tokens + acc.completion_tokens;

        // Final `Done` event so the typed-callback consumer knows the
        // stream is complete and learns the token total.
        if let Some(cb) = callback.as_ref() {
            cb(StreamEvent::Done {
                tokens: total_tokens,
            });
        }

        Ok(ModelResponse {
            content: acc.content,
            usage: Some(TokenUsage {
                prompt_tokens: acc.prompt_tokens,
                completion_tokens: acc.completion_tokens,
                total_tokens,
            }),
            model_name: self.model_name.clone(),
            thinking,
            tool_calls,
            thinking_signature: None,
        })
    }

    /// Process a single parsed stream chunk, updating accumulators and
    /// emitting typed events.
    ///
    /// Event ordering within a chunk: reasoning (if any) → tool calls (if
    /// any) → text (if any). The `Done` event is emitted by the caller
    /// (`handle_stream`) once the stream closes, never here.
    ///
    /// Once the per-stream `MAX_RESPONSE_CHARS` cap trips (`acc.truncated`),
    /// further content / thinking chunks are silently dropped — both from
    /// the accumulator AND from typed-event emission. Tool calls and token
    /// usage are still recorded because those are bounded.
    fn process_stream_chunk(
        json_chunk: &OllamaStreamChunk,
        callback: Option<&StreamCallback>,
        acc: &mut StreamAccumulator,
    ) {
        // Reasoning / thinking content. Always recorded into `acc.thinking`
        // (so `ModelResponse.thinking` stays populated for backward-compat
        // callers); only emitted via typed callback when not hidden.
        if let Some(ref thinking_chunk) = json_chunk.message.thinking
            && !acc.truncated
            && !thinking_chunk.is_empty()
        {
            if let Some(cb) = callback
                && !acc.hide_reasoning_trace
            {
                cb(StreamEvent::Reasoning(ReasoningChunk {
                    text: thinking_chunk.clone(),
                    signature: None,
                }));
            }
            push_capped(
                &mut acc.thinking,
                thinking_chunk,
                &mut acc.truncated,
                MAX_RESPONSE_CHARS,
            );
        }

        // Tool calls — bounded, no cap needed. Emitted as typed events
        // immediately so streaming consumers can react before completion.
        if let Some(ref tool_calls) = json_chunk.message.tool_calls {
            acc.tool_calls.extend(tool_calls.clone());
            if let Some(cb) = callback {
                for tc in tool_calls {
                    cb(StreamEvent::ToolCall(tc.clone()));
                }
            }
        }

        // Regular text content.
        if !json_chunk.message.content.is_empty() && !acc.truncated {
            if let Some(cb) = callback {
                cb(StreamEvent::Text(json_chunk.message.content.clone()));
            }
            push_capped(
                &mut acc.content,
                &json_chunk.message.content,
                &mut acc.truncated,
                MAX_RESPONSE_CHARS,
            );
        }

        // Capture token usage from the `done` chunk.
        if json_chunk.done {
            if let Some(count) = json_chunk.prompt_eval_count {
                acc.prompt_tokens = count;
            }
            if let Some(count) = json_chunk.eval_count {
                acc.completion_tokens = count;
            }
        }
    }

    /// Build the JSON request body shared between `chat` (legacy text
    /// callback) and `chat_typed` (new typed events). Centralizing here
    /// avoids two copies of the message-formatting + tool-filtering +
    /// option-assembly logic.
    fn build_request_body(
        &self,
        messages: &[ChatMessage],
        config: &ModelConfig,
        stream: bool,
    ) -> serde_json::Value {
        let ollama_opts = config.ollama_options();

        let mut json_messages = Vec::new();

        // Ollama doesn't cache; static prompt + MERMAID.md suffix are joined
        // with a `---` separator via combined_system_prompt().
        if let Some(combined) = config.combined_system_prompt() {
            json_messages.push(json!({
                "role": "system",
                "content": combined
            }));
        }

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
            if msg.role == MessageRole::Assistant
                && let Some(ref tool_calls) = msg.tool_calls
            {
                json_msg["tool_calls"] = json!(tool_calls);
            }
            if msg.role == MessageRole::Tool
                && let Some(ref tool_name) = msg.tool_name
            {
                json_msg["tool_name"] = json!(tool_name);
            }
            if let Some(ref images) = msg.images
                && !images.is_empty()
            {
                json_msg["images"] = json!(images);
            }
            json_messages.push(json_msg);
        }

        // Built-in tools, with subagent / cloud-key filtering applied.
        let all_tools = crate::models::ToolRegistry::ollama_tools_cached();
        let no_cloud_key = crate::ollama::get_cloud_api_key().is_none();
        let mut tools: Vec<&serde_json::Value> = all_tools
            .iter()
            .filter(|t| {
                let name = t
                    .pointer("/function/name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("");
                if no_cloud_key && (name == "web_search" || name == "web_fetch") {
                    return false;
                }
                if config.is_subagent && name == "agent" {
                    return false;
                }
                if config.is_subagent && crate::agents::computer_use::GUI_TOOL_NAMES.contains(&name)
                {
                    return false;
                }
                true
            })
            .collect();
        for mcp_tool in &config.mcp_tools {
            tools.push(mcp_tool);
        }

        let mut request_body = json!({
            "model": self.model_name,
            "messages": json_messages,
            "stream": stream,
            "tools": &tools,
        });

        // `think` parameter: most Ollama models accept `think: bool`,
        // but gpt-oss requires a string enum (`"low"|"medium"|"high"`).
        // `think_for_ollama` picks the right shape per model.
        request_body["think"] = think_for_ollama(&self.model_name, config.reasoning);
        tracing::debug!(
            "think reasoning={:?} shape={}",
            config.reasoning,
            if is_gpt_oss(&self.model_name) {
                "string"
            } else {
                "bool"
            }
        );

        tracing::debug!("Sending {} tools to Ollama", tools.len());
        tracing::debug!(
            "Request body tools: {}",
            serde_json::to_string_pretty(&tools).unwrap_or_default()
        );

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

        request_body
    }

    /// POST /api/chat with the given body and return the raw response.
    async fn send_chat(&self, body: &serde_json::Value) -> Result<reqwest::Response> {
        let url = format!("{}/api/chat", self.base_url);
        self.client.post(&url).json(body).send().await.map_err(|e| {
            ModelError::Backend(BackendError::ConnectionFailed {
                backend: "ollama".to_string(),
                url: self.base_url.clone(),
                reason: e.to_string(),
            })
        })
    }

    /// Decode the single non-streaming response body into a `ModelResponse`.
    /// Used by both `chat` (no callback) and `chat_typed` (no callback).
    async fn decode_non_streaming(&self, response: reqwest::Response) -> Result<ModelResponse> {
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
            thinking_signature: None,
        })
    }
}

#[async_trait]
impl Model for OllamaAdapter {
    fn name(&self) -> &str {
        &self.model_name
    }

    fn capabilities(&self) -> &ModelCapabilities {
        &self.capabilities
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
        callback: Option<StreamCallback>,
    ) -> Result<ModelResponse> {
        let stream = callback.is_some();
        let request_body = self.build_request_body(messages, config, stream);
        let response = self.send_chat(&request_body).await?;

        if stream {
            self.handle_stream(response, callback, config.hide_reasoning_trace)
                .await
        } else {
            self.decode_non_streaming(response).await
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

    // Add default Ollama port if missing (only for http; https keeps its default 443).
    // Check the authority portion only (before first '/') to avoid appending the port
    // after a path component (e.g., "http://host/v1" must NOT become "http://host/v1:11434").
    if let Some(after_scheme) = normalized.strip_prefix("http://") {
        let (authority, path) = match after_scheme.find('/') {
            Some(i) => (&after_scheme[..i], &after_scheme[i..]),
            None => (after_scheme, ""),
        };
        if !authority.contains(':') {
            normalized = format!("http://{}:11434{}", authority, path);
        }
    }
    // For https:// without a port, don't add :11434 — the default port (443) is correct

    normalized
}

#[cfg(test)]
mod tests {
    use super::{TRUNCATION_MARKER, is_gpt_oss, normalize_url, push_capped};

    // --- push_capped: response-size cap for streaming accumulators ---

    #[test]
    fn push_capped_under_cap_appends_normally() {
        let mut buf = String::new();
        let mut truncated = false;
        push_capped(&mut buf, "hello", &mut truncated, 100);
        push_capped(&mut buf, " world", &mut truncated, 100);
        assert_eq!(buf, "hello world");
        assert!(!truncated);
    }

    #[test]
    fn push_capped_truncates_once_then_drops_chunks() {
        let mut buf = String::new();
        let mut truncated = false;
        let cap = 32;
        // First chunk overflows.
        push_capped(&mut buf, &"a".repeat(200), &mut truncated, cap);
        assert!(truncated);
        assert!(buf.ends_with(TRUNCATION_MARKER));
        let len_after_first = buf.len();
        // Subsequent chunks are silently dropped.
        push_capped(&mut buf, &"b".repeat(200), &mut truncated, cap);
        push_capped(&mut buf, "tail", &mut truncated, cap);
        assert_eq!(buf.len(), len_after_first);
        assert_eq!(buf.matches(TRUNCATION_MARKER).count(), 1);
    }

    #[test]
    fn push_capped_respects_char_boundary_for_cjk() {
        let mut buf = String::new();
        let mut truncated = false;
        // 4 bytes lands inside the second 3-byte 你; floor must back off to 3.
        push_capped(&mut buf, "你你你你", &mut truncated, 4);
        let body = &buf[..buf.find('\n').unwrap()];
        assert_eq!(body, "你");
        assert!(buf.ends_with(TRUNCATION_MARKER));
    }

    #[test]
    fn test_normalize_url_bare_host() {
        assert_eq!(normalize_url("localhost"), "http://localhost:11434");
    }

    #[test]
    fn test_normalize_url_http_no_port() {
        assert_eq!(normalize_url("http://localhost"), "http://localhost:11434");
    }

    #[test]
    fn test_normalize_url_http_with_port() {
        assert_eq!(
            normalize_url("http://localhost:11434"),
            "http://localhost:11434"
        );
    }

    #[test]
    fn test_normalize_url_custom_port() {
        assert_eq!(normalize_url("http://host:8080"), "http://host:8080");
    }

    #[test]
    fn test_normalize_url_with_path_no_port() {
        assert_eq!(
            normalize_url("http://ollama.example.com/v1"),
            "http://ollama.example.com:11434/v1"
        );
    }

    #[test]
    fn test_normalize_url_with_path_and_port() {
        assert_eq!(
            normalize_url("http://ollama.example.com:8080/v1"),
            "http://ollama.example.com:8080/v1"
        );
    }

    #[test]
    fn test_normalize_url_https_no_port_added() {
        assert_eq!(
            normalize_url("https://ollama.example.com"),
            "https://ollama.example.com"
        );
    }

    #[test]
    fn test_normalize_url_replaces_0000() {
        assert_eq!(
            normalize_url("http://0.0.0.0:11434"),
            "http://127.0.0.1:11434"
        );
    }

    // --- think mapping from ReasoningLevel (Step 4) ---

    use super::OllamaAdapter;
    use crate::models::config::{BackendConfig, ModelConfig};
    use crate::models::reasoning::ReasoningLevel;
    use crate::models::types::ChatMessage;
    use std::sync::Arc;

    async fn make_adapter() -> OllamaAdapter {
        // `OllamaAdapter::new` builds an HTTP client but does NOT contact
        // the server, so this works offline.
        OllamaAdapter::new("test-model", Arc::new(BackendConfig::default()))
            .await
            .expect("adapter")
    }

    #[tokio::test]
    async fn ollama_request_body_omits_think_when_reasoning_none() {
        let adapter = make_adapter().await;
        let config = ModelConfig {
            reasoning: ReasoningLevel::None,
            ..Default::default()
        };
        let messages = vec![ChatMessage::user("hi")];

        let body = adapter.build_request_body(&messages, &config, false);
        assert_eq!(body["think"], serde_json::json!(false));
    }

    #[tokio::test]
    async fn ollama_request_body_sets_think_true_for_low_reasoning() {
        let adapter = make_adapter().await;
        let config = ModelConfig {
            reasoning: ReasoningLevel::Low,
            ..Default::default()
        };
        let messages = vec![ChatMessage::user("hi")];

        let body = adapter.build_request_body(&messages, &config, false);
        assert_eq!(body["think"], serde_json::json!(true));
    }

    #[tokio::test]
    async fn ollama_request_body_sets_think_true_for_max_reasoning() {
        let adapter = make_adapter().await;
        let config = ModelConfig {
            reasoning: ReasoningLevel::Max,
            ..Default::default()
        };
        let messages = vec![ChatMessage::user("hi")];

        let body = adapter.build_request_body(&messages, &config, false);
        assert_eq!(body["think"], serde_json::json!(true));
    }

    /// gpt-oss models require `think` as a STRING enum (not bool).
    /// Sending a bool silently uses the default effort; sending the
    /// wrong shape to a non-gpt-oss model 400s. `think_for_ollama` gates
    /// on model name.
    async fn make_gpt_oss_adapter() -> OllamaAdapter {
        OllamaAdapter::new("gpt-oss:20b", Arc::new(BackendConfig::default()))
            .await
            .expect("adapter")
    }

    #[tokio::test]
    async fn ollama_request_body_sets_think_low_for_gpt_oss_none() {
        let adapter = make_gpt_oss_adapter().await;
        let config = ModelConfig {
            reasoning: ReasoningLevel::None,
            ..Default::default()
        };
        let body = adapter.build_request_body(&[ChatMessage::user("hi")], &config, false);
        // gpt-oss can't truly disable; None collapses to "low".
        assert_eq!(body["think"], serde_json::json!("low"));
    }

    #[tokio::test]
    async fn ollama_request_body_sets_think_medium_for_gpt_oss_medium() {
        let adapter = make_gpt_oss_adapter().await;
        let config = ModelConfig {
            reasoning: ReasoningLevel::Medium,
            ..Default::default()
        };
        let body = adapter.build_request_body(&[ChatMessage::user("hi")], &config, false);
        assert_eq!(body["think"], serde_json::json!("medium"));
    }

    #[tokio::test]
    async fn ollama_request_body_sets_think_high_for_gpt_oss_max() {
        let adapter = make_gpt_oss_adapter().await;
        let config = ModelConfig {
            reasoning: ReasoningLevel::Max,
            ..Default::default()
        };
        let body = adapter.build_request_body(&[ChatMessage::user("hi")], &config, false);
        // Max / High / XHigh all snap to the gpt-oss top tier "high".
        assert_eq!(body["think"], serde_json::json!("high"));
    }

    #[tokio::test]
    async fn ollama_request_body_sets_think_high_for_gpt_oss_xhigh() {
        let adapter = make_gpt_oss_adapter().await;
        let config = ModelConfig {
            reasoning: ReasoningLevel::XHigh,
            ..Default::default()
        };
        let body = adapter.build_request_body(&[ChatMessage::user("hi")], &config, false);
        assert_eq!(body["think"], serde_json::json!("high"));
    }

    #[test]
    fn is_gpt_oss_matches_prefix_case_insensitive() {
        assert!(is_gpt_oss("gpt-oss:20b"));
        assert!(is_gpt_oss("gpt-oss:120b-cloud"));
        assert!(is_gpt_oss("GPT-OSS:20b"));
        assert!(!is_gpt_oss("qwen3-coder:30b"));
        assert!(!is_gpt_oss("gpt-4o"));
    }

    /// Step 5h: Ollama doesn't cache, so the dynamic MERMAID.md suffix is
    /// concatenated onto the static system message with a `---` separator.
    /// Both halves reach the model in one system message payload.
    #[tokio::test]
    async fn ollama_request_body_concats_dynamic_suffix_to_system_message() {
        let adapter = make_adapter().await;
        let config = ModelConfig {
            system_prompt: Some("You are Mermaid.".to_string()),
            dynamic_system_suffix: Some("Project rule: always snake_case.".to_string()),
            ..Default::default()
        };
        let messages = vec![ChatMessage::user("hi")];

        let body = adapter.build_request_body(&messages, &config, false);
        let messages_arr = body["messages"].as_array().expect("messages array");
        assert_eq!(messages_arr[0]["role"], "system");
        let content = messages_arr[0]["content"].as_str().unwrap();
        assert!(content.contains("You are Mermaid."));
        assert!(content.contains("Project rule: always snake_case."));
        assert!(content.contains("---"));
    }
}
