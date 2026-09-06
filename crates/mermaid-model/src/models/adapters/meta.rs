//! Meta Model API adapter, on the Responses endpoint.
//!
//! Unlike Meta's OpenAI-compatible Chat Completions surface, Responses can
//! carry encrypted reasoning across tool turns. Mermaid uses stateless
//! replay: every request sets `store: false`, asks for
//! `reasoning.encrypted_content`, and persists the returned output items on
//! the assistant message, replaying them verbatim next turn.
//!
//! The fifth adapter, and the last to arrive. It spent its first life as a
//! `ModelProvider` in the CLI crate, hand-rolling everything a `Model`
//! adapter already had: its own SSE loop, its own reassembly cap, its own
//! status-error handler, its own cancellation check. What kept it there was
//! one dependency — it built its request straight from `ChatRequest`, which
//! lives in `mermaid-domain`, one crate ABOVE this one. Taking
//! `&[ChatMessage]` and a `ModelConfig` like its four siblings is the whole
//! of what moving it required.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde_json::{Value, json};

use super::accumulator::{CappedText, http_error, parse_tool_args, slot_in_bounds};
use crate::models::adapters::driver::{Flow, Framing, StreamProtocol, drive_stream};
use crate::models::capabilities::ModelCapabilities;
use crate::models::config::ModelConfig;
use crate::models::error::{BackendError, ModelError, Result};
use crate::models::reasoning::{
    ReasoningCapability, ReasoningChunk, ReasoningLevel, nearest_effort,
};
use crate::models::stream::{StreamEvent, StreamSink};
use crate::models::tool_call::{FunctionCall, ToolCall};
use crate::models::traits::Model;
use crate::models::types::{
    ChatMessage, FinishReason, MessageRole, MetaResponseItem, ModelResponse, ProviderContinuation,
    TokenUsage,
};

/// Meta's Responses-API root, and the env var its key lives in.
pub const DEFAULT_BASE_URL: &str = "https://api.meta.ai/v1";
pub const DEFAULT_API_KEY_ENV: &str = "MODEL_API_KEY";

pub struct MetaAdapter {
    client: Client,
    base_url: String,
    api_key: String,
    model_name: String,
    extra_headers: HashMap<String, String>,
    capabilities: ModelCapabilities,
}

impl MetaAdapter {
    /// Build the Responses-API adapter for a Meta model.
    ///
    /// # Errors
    ///
    /// Only the HTTP client build, as [`BackendError::ConnectionFailed`]. The
    /// API is not contacted here; a `model_name` outside the `muse-spark`
    /// family is not an error either — it simply advertises no documented
    /// context or output limits.
    pub fn new(
        api_key: String,
        model_name: String,
        base_url: String,
        extra_headers: HashMap<String, String>,
    ) -> Result<Self> {
        let client = Client::builder()
            .pool_max_idle_per_host(10)
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(60))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|error| {
                ModelError::Backend(BackendError::ConnectionFailed {
                    backend: "meta".to_string(),
                    url: base_url.clone(),
                    reason: error.to_string(),
                })
            })?;
        // Prefix, not exact-id: a future muse-spark-1.2 should inherit the
        // documented family limits instead of regressing to "unknown".
        let muse_spark = model_name.to_ascii_lowercase().starts_with("muse-spark");
        // The one sanctioned static-window exception (see capabilities.rs's
        // module doc): Meta documents the muse-spark family limits and
        // exposes no endpoint to discover them live.
        let capabilities = ModelCapabilities {
            max_context_tokens: muse_spark
                .then_some(crate::constants::META_MUSE_SPARK_CONTEXT_WINDOW),
            max_output_tokens: muse_spark
                .then_some(crate::constants::META_MUSE_SPARK_MAX_OUTPUT_TOKENS),
            ..ModelCapabilities::advertised(
                true,
                ReasoningCapability::Levels(meta_reasoning_levels()),
            )
            .with_provider_continuation()
        };
        Ok(Self {
            client,
            base_url,
            api_key,
            model_name,
            extra_headers,
            capabilities,
        })
    }

    async fn send_chat(&self, body: &Value) -> Result<reqwest::Response> {
        let url = format!("{}/responses", self.base_url.trim_end_matches('/'));
        let mut builder = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .header("Accept", "text/event-stream")
            .json(body);
        for (name, value) in &self.extra_headers {
            builder = builder.header(name, value);
        }
        builder.send().await.map_err(|error| {
            ModelError::Backend(BackendError::ConnectionFailed {
                backend: "meta".to_string(),
                url,
                reason: error.to_string(),
            })
        })
    }
}

#[async_trait]
impl Model for MetaAdapter {
    fn name(&self) -> &str {
        &self.model_name
    }

    fn capabilities(&self) -> &ModelCapabilities {
        &self.capabilities
    }

    async fn chat(
        &self,
        messages: &[ChatMessage],
        config: &ModelConfig,
        sink: Option<StreamSink>,
    ) -> Result<ModelResponse> {
        // Responses is a streaming-only surface here: mermaid always asks for
        // `stream: true` because that is the only shape the encrypted
        // reasoning items arrive in. A sink-less call still drives the same
        // stream, it just drops the events.
        let body = build_request_body(messages, config, &self.model_name);
        let response = self.send_chat(&body).await?;
        if !response.status().is_success() {
            return Err(meta_http_error(response).await);
        }
        drive_stream(
            response.bytes_stream(),
            MetaStream::new(self.model_name.clone()),
            sink.as_ref(),
        )
        .await
    }
}

/// Meta's Responses event stream as a [`StreamProtocol`].
///
/// The terminal frame is explicit (`response.completed` / `.incomplete`) and
/// carries the whole response object, so unlike the other four this protocol
/// accumulates almost nothing during the stream — the text deltas are for
/// display, and the authoritative output items arrive at the end.
pub(crate) struct MetaStream {
    model_name: String,
    /// Tool calls already emitted, by `call_id`. The same call arrives twice
    /// — once as `response.output_item.done`, once in the terminal
    /// response's `output` — and the agent must not run it twice.
    emitted_calls: HashSet<String>,
    tool_calls: Vec<ToolCall>,
    content: CappedText,
    thinking: CappedText,
    /// The terminal frame's `response` object plus which event carried it.
    /// `None` until then, which is exactly what makes a cut body detectable.
    terminal: Option<(Value, String)>,
}

impl MetaStream {
    pub(crate) fn new(model_name: String) -> Self {
        Self {
            model_name,
            emitted_calls: HashSet::new(),
            tool_calls: Vec::new(),
            content: CappedText::default(),
            thinking: CappedText::default(),
            terminal: None,
        }
    }

    /// Record and emit a function-call output item, once per `call_id`.
    fn take_tool_call(&mut self, item: &Value, out: &mut Vec<StreamEvent>) {
        let Some(call) = tool_call_from_item(item) else {
            return;
        };
        let call_id = call.id.clone().unwrap_or_default();
        if !slot_in_bounds(self.tool_calls.len()) {
            // A stream can mint a fresh call_id per frame forever; past the
            // bound the call is dropped rather than accumulated.
            tracing::warn!("meta stream exceeded the tool-call bound; ignoring further calls");
            return;
        }
        if self.emitted_calls.insert(call_id) {
            self.tool_calls.push(call.clone());
            out.push(StreamEvent::ToolCall(call));
        }
    }
}

impl StreamProtocol for MetaStream {
    const FRAMING: Framing = Framing::Sse;

    fn on_frame(&mut self, frame: &str, out: &mut Vec<StreamEvent>) -> Result<Flow> {
        let event: Value = serde_json::from_str(frame).map_err(|error| ModelError::ParseError {
            message: format!("failed to parse Meta Responses event: {error}"),
            raw: None,
        })?;
        let event_type = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match event_type {
            "response.output_text.delta" => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str)
                    && self.content.accepting()
                {
                    self.content.push(delta);
                    out.push(StreamEvent::Text(delta.to_string()));
                }
            },
            "response.reasoning_summary_text.delta" => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str)
                    && self.thinking.accepting()
                {
                    self.thinking.push(delta);
                    out.push(StreamEvent::Reasoning(ReasoningChunk {
                        text: delta.to_string(),
                        signature: None,
                    }));
                }
            },
            "response.output_item.done" => {
                if let Some(item) = event.get("item") {
                    self.take_tool_call(item, out);
                }
            },
            "response.completed" | "response.incomplete" => {
                let response = event
                    .get("response")
                    .ok_or_else(|| ModelError::ParseError {
                        message: format!("Meta {event_type} event omitted response"),
                        raw: None,
                    })?
                    .clone();
                // The terminal `output` repeats every item, including calls
                // already streamed; `take_tool_call` dedupes by `call_id`.
                for item in response
                    .get("output")
                    .and_then(Value::as_array)
                    .unwrap_or(&Vec::new())
                {
                    self.take_tool_call(item, out);
                }
                self.terminal = Some((response, event_type.to_string()));
                return Ok(Flow::Stop);
            },
            "response.failed" | "error" => return Err(meta_failure(&event)),
            "response.cancelled" => {
                return Err(ModelError::StreamError(
                    "Meta cancelled the response".to_string(),
                ));
            },
            _ => {},
        }
        Ok(Flow::Continue)
    }

    fn finish(self, _out: &mut Vec<StreamEvent>) -> Result<ModelResponse> {
        // F56, Meta's spelling: the terminal event is explicit and carries
        // everything that round-trips, so its absence means the connection
        // dropped — never a short success.
        let Some((response, event_type)) = self.terminal else {
            return Err(ModelError::StreamError(
                "Meta Responses stream closed before a terminal event".to_string(),
            ));
        };

        let output = response
            .get("output")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let continuation = ProviderContinuation::MetaResponses {
            output: output
                .into_iter()
                .filter(meta_item_is_replayable)
                .map(MetaResponseItem::from_wire)
                .collect(),
        };
        let stop_reason = meta_finish_reason(&response, &event_type, !self.tool_calls.is_empty());

        Ok(ModelResponse {
            content: self.content.into_string(),
            usage: response.get("usage").map(meta_usage),
            model_name: self.model_name,
            stop_reason: Some(stop_reason),
            thinking: (!self.thinking.is_empty()).then(|| self.thinking.into_string()),
            tool_calls: (!self.tool_calls.is_empty()).then_some(self.tool_calls),
            provider_continuation: Some(continuation),
        })
    }
}

fn tool_call_from_item(item: &Value) -> Option<ToolCall> {
    if item.get("type").and_then(Value::as_str) != Some("function_call") {
        return None;
    }
    let call_id = item.get("call_id")?.as_str()?.to_string();
    let name = item.get("name")?.as_str()?.to_string();
    let raw_arguments = item
        .get("arguments")
        .and_then(Value::as_str)
        .unwrap_or("{}");
    let arguments = parse_tool_args(&name, raw_arguments.to_string());
    Some(ToolCall {
        id: Some(call_id),
        function: FunctionCall { name, arguments },
    })
}

pub(crate) fn build_request_body(
    messages: &[ChatMessage],
    config: &ModelConfig,
    model_name: &str,
) -> Value {
    let effort = nearest_effort(config.reasoning, &meta_reasoning_levels())
        .unwrap_or(ReasoningLevel::Minimal);
    let mut body = json!({
        "model": model_name,
        "input": messages_to_input(messages),
        "stream": true,
        "store": false,
        "include": ["reasoning.encrypted_content"],
        "reasoning": {
            "effort": meta_effort(effort),
            "summary": "auto",
        },
    });
    // Muse is tuned for Meta's 1.0 default. Mermaid's global 0.7 default was
    // chosen for other providers, so omit it here unless the user changed it.
    if (config.temperature - crate::constants::DEFAULT_TEMPERATURE).abs() > f32::EPSILON {
        body["temperature"] = json!(config.temperature);
    }
    let instructions = combined_instructions(config);
    if !instructions.is_empty() {
        body["instructions"] = Value::String(instructions);
    }
    let tools = meta_tools(&config.tools);
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    if config.max_tokens > 0 {
        let limit = config
            .resolved_max_output
            .map_or(config.max_tokens, |max| config.max_tokens.min(max));
        body["max_output_tokens"] = json!(limit);
    }
    body
}

/// Unwrap the OpenAI `{"type":"function","function":{...}}` envelope that
/// `ToolDefinition::to_openai_json` produces into the flat shape Responses
/// wants. Same job `to_anthropic_tools` does one file over.
fn meta_tools(openai_tools: &[Value]) -> Vec<Value> {
    openai_tools
        .iter()
        .filter_map(|tool| {
            let function = tool.get("function")?;
            Some(json!({
                "type": "function",
                "name": function.get("name")?,
                "description": function.get("description").cloned().unwrap_or(Value::Null),
                "parameters": function.get("parameters").cloned().unwrap_or_else(|| json!({})),
            }))
        })
        .collect()
}

fn messages_to_input(messages: &[ChatMessage]) -> Vec<Value> {
    let mut input = Vec::new();
    for message in messages {
        if message.role == MessageRole::Assistant
            && let Some(output) = message
                .provider_continuation
                .as_ref()
                .and_then(ProviderContinuation::meta_output)
        {
            input.extend(meta_output_to_input(output));
            continue;
        }
        match message.role {
            MessageRole::Tool => input.push(json!({
                "type": "function_call_output",
                "call_id": message.tool_call_id.clone().unwrap_or_default(),
                "output": message.content,
            })),
            MessageRole::User => input.push(input_message(message, "user", "input_text")),
            MessageRole::System => input.push(input_message(message, "system", "input_text")),
            MessageRole::Assistant => {
                if !message.content.is_empty() {
                    let mut assistant = input_message(message, "assistant", "output_text");
                    if message
                        .tool_calls
                        .as_ref()
                        .is_some_and(|calls| !calls.is_empty())
                    {
                        assistant["phase"] = json!("commentary");
                    }
                    input.push(assistant);
                }
                for call in message.tool_calls.iter().flatten() {
                    input.push(json!({
                        "type": "function_call",
                        "call_id": call.id.clone().unwrap_or_default(),
                        "name": call.function.name,
                        "arguments": serde_json::to_string(&call.function.arguments)
                            .unwrap_or_else(|_| "{}".to_string()),
                        "status": "completed",
                    }));
                }
            },
        }
    }
    input
}

fn meta_output_to_input(output: &[MetaResponseItem]) -> Vec<Value> {
    let mut input = output
        .iter()
        .map(MetaResponseItem::to_wire)
        .collect::<Vec<_>>();
    // Meta rejects a replayed reasoning item followed directly by the next user
    // turn. A rare reasoning-only response therefore needs a minimal assistant
    // message before the conversation continues.
    if input
        .last()
        .and_then(|item| item.get("type"))
        .and_then(Value::as_str)
        == Some("reasoning")
    {
        input.push(json!({
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "I will continue."}]
        }));
    }
    input
}

fn input_message(message: &ChatMessage, role: &str, text_type: &str) -> Value {
    let mut content = Vec::new();
    if !message.content.is_empty() {
        content.push(json!({"type": text_type, "text": message.content}));
    }
    if role == "user" {
        for image in message.images.iter().flatten() {
            content.push(json!({
                "type": "input_image",
                "image_url": format!("data:image/png;base64,{image}"),
            }));
        }
    }
    json!({"type": "message", "role": role, "content": content})
}

/// The static system prompt and the project's `MERMAID.md` suffix, joined the
/// way Responses wants them: one `instructions` string, blank line between.
fn combined_instructions(config: &ModelConfig) -> String {
    let system = config.system_prompt.as_deref().unwrap_or_default();
    match config
        .dynamic_system_suffix
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        Some(suffix) if !system.is_empty() => format!("{system}\n\n{suffix}"),
        Some(suffix) => suffix.to_string(),
        None => system.to_string(),
    }
}

fn meta_reasoning_levels() -> Vec<ReasoningLevel> {
    vec![
        ReasoningLevel::Minimal,
        ReasoningLevel::Low,
        ReasoningLevel::Medium,
        ReasoningLevel::High,
        ReasoningLevel::XHigh,
    ]
}

fn meta_effort(level: ReasoningLevel) -> &'static str {
    match level {
        ReasoningLevel::None | ReasoningLevel::Minimal => "minimal",
        ReasoningLevel::Low => "low",
        ReasoningLevel::Medium => "medium",
        ReasoningLevel::High => "high",
        ReasoningLevel::XHigh | ReasoningLevel::Max => "xhigh",
    }
}

fn meta_item_is_replayable(item: &Value) -> bool {
    item.get("type").and_then(Value::as_str) != Some("reasoning")
        || item
            .get("encrypted_content")
            .and_then(Value::as_str)
            .is_some()
}

fn meta_usage(value: &Value) -> TokenUsage {
    let input = usize_field(value, "input_tokens");
    let output = usize_field(value, "output_tokens");
    let cached = value
        .get("input_tokens_details")
        .map(|details| usize_field(details, "cached_tokens"))
        .unwrap_or_default();
    let reasoning = value
        .get("output_tokens_details")
        .map(|details| usize_field(details, "reasoning_tokens"))
        .unwrap_or_default();
    // Responses-API wire counts nest cached inside input_tokens and
    // reasoning inside output_tokens; carve both out so the shared
    // TokenUsage components stay disjoint (matches openai_compat).
    TokenUsage::provider(
        input.saturating_sub(cached),
        output.saturating_sub(reasoning),
    )
    .with_cached_input(cached)
    .with_reasoning_output(reasoning)
}

fn usize_field(value: &Value, key: &str) -> usize {
    value
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or_default()
}

fn meta_finish_reason(response: &Value, event_type: &str, has_tools: bool) -> FinishReason {
    let incomplete_reason = response
        .get("incomplete_details")
        .and_then(|details| details.get("reason"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if event_type == "response.incomplete"
        || response.get("status").and_then(Value::as_str) == Some("incomplete")
    {
        if incomplete_reason.contains("max_output") || incomplete_reason.contains("length") {
            return FinishReason::Length;
        }
        if incomplete_reason.contains("content_filter") || incomplete_reason.contains("safety") {
            return FinishReason::ContentFilter;
        }
        return FinishReason::Other(if incomplete_reason.is_empty() {
            "incomplete".to_string()
        } else {
            incomplete_reason.to_string()
        });
    }
    if has_tools {
        FinishReason::ToolUse
    } else {
        FinishReason::Stop
    }
}

fn meta_failure(event: &Value) -> ModelError {
    let error = event
        .get("response")
        .and_then(|response| response.get("error"))
        .or_else(|| event.get("error"));
    let message = error
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .or_else(|| event.get("message").and_then(Value::as_str))
        .unwrap_or("Meta Responses request failed");
    ModelError::Backend(BackendError::ProviderError {
        provider: "meta".to_string(),
        code: error
            .and_then(|error| error.get("code"))
            .and_then(Value::as_str)
            .map(str::to_string),
        message: crate::utils::redact_secrets(message),
        debug: crate::models::error::ResponseDebugContext::default(),
    })
}

/// Meta's own status-error handler, kept rather than shared: it redacts the
/// body, and a Responses 4xx routinely echoes the `Authorization` header
/// back inside the message.
async fn meta_http_error(response: reqwest::Response) -> ModelError {
    http_error(response, "Meta request failed").await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::types::ChatMessageKind;

    fn config() -> ModelConfig {
        ModelConfig {
            model: "meta/muse-spark-1.1".to_string(),
            temperature: crate::constants::DEFAULT_TEMPERATURE,
            max_tokens: 200_000,
            reasoning: ReasoningLevel::Max,
            system_prompt: Some("system".to_string()),
            dynamic_system_suffix: Some("project".to_string()),
            tools: vec![json!({
                "type": "function",
                "function": {
                    "name": "read_file",
                    "description": "Read a file",
                    "parameters": {"type": "object"},
                }
            })],
            resolved_max_output: Some(crate::constants::META_MUSE_SPARK_MAX_OUTPUT_TOKENS),
            ..Default::default()
        }
    }

    fn messages() -> Vec<ChatMessage> {
        vec![ChatMessage::user("hello").with_images(vec!["PNG".to_string()])]
    }

    #[test]
    fn request_uses_stateless_encrypted_replay_shape() {
        let body = build_request_body(&messages(), &config(), "muse-spark-1.1");
        assert_eq!(body["store"], false);
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(body["reasoning"]["effort"], "xhigh");
        assert_eq!(body["reasoning"]["summary"], "auto");
        assert_eq!(
            body["max_output_tokens"],
            crate::constants::META_MUSE_SPARK_MAX_OUTPUT_TOKENS
        );
        assert_eq!(body["instructions"], "system\n\nproject");
        assert_eq!(body["tools"][0]["name"], "read_file");
        assert!(body.get("temperature").is_none());
        assert!(body.get("previous_response_id").is_none());
        assert!(body.get("tool_choice").is_none());
        assert_eq!(body["input"][0]["content"][1]["type"], "input_image");
    }

    #[test]
    fn tool_definitions_lose_the_openai_function_wrapper() {
        // Responses takes `name`/`parameters` flat; the config carries them
        // in the Chat Completions envelope every other adapter reads.
        let body = build_request_body(&messages(), &config(), "muse-spark-1.1");
        let tool = &body["tools"][0];
        assert_eq!(tool["type"], "function");
        assert_eq!(tool["name"], "read_file");
        assert_eq!(tool["description"], "Read a file");
        assert_eq!(tool["parameters"], json!({"type": "object"}));
        assert!(tool.get("function").is_none());
    }

    /// Adapter contract (see `MessageAudience`): harness steering must reach
    /// the model. The Responses API carries system-role input messages, so the
    /// reminder passes through in place at the tail.
    #[test]
    fn model_directed_system_messages_reach_the_wire_in_place() {
        let mut msgs = messages();
        let mut nudge = ChatMessage::system("Reminder: plan mode is active.");
        nudge.kind = ChatMessageKind::RecoveryNudge;
        msgs.push(nudge);
        let body = build_request_body(&msgs, &config(), "muse-spark-1.1");

        let input = body["input"].as_array().expect("input array");
        let last = input.last().expect("non-empty");
        assert_eq!(last["role"], "system");
        assert!(
            serde_json::to_string(&last["content"])
                .unwrap()
                .contains("plan mode is active"),
        );
    }

    #[test]
    fn none_reasoning_maps_to_minimal_and_auto_budget_is_omitted() {
        let cfg = ModelConfig {
            reasoning: ReasoningLevel::None,
            max_tokens: 0,
            ..config()
        };
        let body = build_request_body(&messages(), &cfg, "muse-spark-1.1");
        assert_eq!(body["reasoning"]["effort"], "minimal");
        assert!(body.get("max_output_tokens").is_none());
    }

    #[test]
    fn continuation_replays_order_phase_and_encrypted_content() {
        let output = vec![
            MetaResponseItem::from_wire(json!({
                "type": "reasoning",
                "id": "rs_1",
                "summary": [],
                "encrypted_content": "eyJcipher.payload.signature"
            })),
            MetaResponseItem::from_wire(json!({
                "type": "message",
                "role": "assistant",
                "phase": "commentary",
                "content": [{"type": "output_text", "text": "checking"}]
            })),
            MetaResponseItem::from_wire(json!({
                "type": "function_call",
                "call_id": "call_1",
                "name": "read_file",
                "arguments": "{\"path\":\"README.md\"}"
            })),
        ];
        let message = ChatMessage::assistant("checking")
            .with_provider_continuation(ProviderContinuation::MetaResponses { output });
        let input = messages_to_input(&[
            message,
            ChatMessage::tool("call_1", "read_file", "contents"),
        ]);
        assert_eq!(input[0]["type"], "reasoning");
        assert_eq!(input[0]["encrypted_content"], "eyJcipher.payload.signature");
        assert_eq!(input[1]["phase"], "commentary");
        assert_eq!(input[2]["call_id"], "call_1");
        assert_eq!(input[3]["type"], "function_call_output");
    }

    #[test]
    fn reasoning_only_replay_gets_required_assistant_follower() {
        let output = vec![MetaResponseItem::from_wire(json!({
            "type": "reasoning",
            "id": "rs_1",
            "summary": [],
            "encrypted_content": "ciphertext"
        }))];
        let input = meta_output_to_input(&output);
        assert_eq!(input[0]["type"], "reasoning");
        assert_eq!(input[1]["type"], "message");
        assert_eq!(input[1]["role"], "assistant");
    }

    #[test]
    fn parses_tool_calls_usage_and_finish_reasons() {
        let call = tool_call_from_item(&json!({
            "type": "function_call",
            "call_id": "call_7",
            "name": "execute_command",
            "arguments": "{\"cmd\":\"pwd\"}"
        }))
        .expect("function_call parses");
        assert_eq!(call.id.as_deref(), Some("call_7"));
        assert_eq!(call.function.arguments["cmd"], "pwd");

        let usage = meta_usage(&json!({
            "input_tokens": 100,
            "output_tokens": 40,
            "total_tokens": 140,
            "input_tokens_details": {"cached_tokens": 20},
            "output_tokens_details": {"reasoning_tokens": 15}
        }));
        assert_eq!(usage.prompt_tokens, 80, "cached carved out of input");
        assert_eq!(
            usage.completion_tokens, 25,
            "reasoning carved out of output"
        );
        assert_eq!(usage.total_tokens(), 140);
        assert_eq!(usage.cached_input_tokens, 20);
        assert_eq!(usage.reasoning_output_tokens, 15);
        assert_eq!(
            meta_finish_reason(
                &json!({"status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"}}),
                "response.incomplete",
                false,
            ),
            FinishReason::Length
        );
        assert_eq!(
            meta_finish_reason(&json!({}), "response.completed", true),
            FinishReason::ToolUse
        );
    }

    #[test]
    fn failed_event_is_redacted_and_structured() {
        let error = meta_failure(&json!({
            "type": "response.failed",
            "response": {
                "error": {
                    "code": "bad_request",
                    "message": "Authorization: Bearer abcdef123456ghijkl"
                }
            }
        }));
        let rendered = error.to_string();
        assert!(rendered.contains("bad_request"));
        assert!(rendered.contains("[REDACTED]"));
        assert!(!rendered.contains("abcdef123456ghijkl"));
    }

    #[test]
    fn a_call_streamed_early_is_not_run_twice() {
        // The same function call arrives as `response.output_item.done` AND
        // again inside the terminal response's `output`. Emitting it twice
        // would make the agent run the tool twice.
        let function_call = json!({
            "type": "function_call",
            "call_id": "call_1",
            "name": "read_file",
            "arguments": "{\"path\":\"README.md\"}",
            "status": "completed"
        });
        let mut protocol = MetaStream::new("muse-spark-1.1".to_string());
        let mut events = Vec::new();
        let item_done =
            json!({"type": "response.output_item.done", "item": function_call}).to_string();
        let completed = json!({
            "type": "response.completed",
            "response": {"status": "completed", "output": [function_call]}
        })
        .to_string();
        protocol
            .on_frame(&item_done, &mut events)
            .expect("item.done");
        let flow = protocol
            .on_frame(&completed, &mut events)
            .expect("completed");
        assert_eq!(flow, Flow::Stop);
        let response = protocol.finish(&mut events).expect("finish");
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, StreamEvent::ToolCall(_)))
                .count(),
            1
        );
        assert_eq!(response.tool_calls.expect("tool calls").len(), 1);
    }

    /// Each `response.output_item.done` may carry a fresh `call_id`; the
    /// tool-call list is bounded by `slot_in_bounds` so a hostile stream
    /// cannot grow it (and the agent loop's work list) without limit.
    #[test]
    fn tool_calls_past_the_bound_are_ignored() {
        let mut stream = MetaStream::new("muse-spark-test".to_string());
        let mut out = Vec::new();
        for i in 0..(crate::constants::MAX_TOOL_CALLS + 50) {
            let frame = format!(
                r#"{{"type":"response.output_item.done","item":{{"type":"function_call","call_id":"call_{i}","name":"read_file","arguments":"{{}}","status":"completed"}}}}"#
            );
            stream.on_frame(&frame, &mut out).unwrap();
        }
        assert_eq!(stream.tool_calls.len(), crate::constants::MAX_TOOL_CALLS);
        assert_eq!(out.len(), crate::constants::MAX_TOOL_CALLS);
    }
}
