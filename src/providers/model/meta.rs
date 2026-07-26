//! Meta Model API provider using the Responses endpoint.
//!
//! Unlike Meta's OpenAI-compatible Chat Completions surface, Responses can
//! carry encrypted reasoning across tool turns. Mermaid uses stateless replay:
//! every request sets `store: false`, asks for `reasoning.encrypted_content`,
//! and persists the returned output items on the assistant message.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client;
use serde_json::{Value, json};

use crate::domain::ChatRequest;
use crate::models::tool_call::{FunctionCall, ToolCall};
use crate::models::{
    BackendError, FinishReason, MessageRole, MetaResponseItem, ModelError, ProviderContinuation,
    ReasoningCapability, ReasoningChunk, ReasoningLevel, Result, TokenUsage, nearest_effort,
};
use crate::utils::drain_sse_events;

use super::super::capabilities::Capabilities;
use super::super::ctx::{FinalResponse, StreamContext, StreamEvent};
use super::ModelProvider;

pub const DEFAULT_BASE_URL: &str = "https://api.meta.ai/v1";
pub const DEFAULT_API_KEY_ENV: &str = "MODEL_API_KEY";

pub struct MetaProvider {
    client: Client,
    base_url: String,
    api_key: String,
    model_name: String,
    extra_headers: HashMap<String, String>,
    capabilities: Capabilities,
}

impl MetaProvider {
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
        let capabilities = Capabilities {
            supports_tools: true,
            supports_vision: true,
            supports_reasoning: ReasoningCapability::Levels(meta_reasoning_levels()),
            max_context_tokens: muse_spark
                .then_some(crate::constants::META_MUSE_SPARK_CONTEXT_WINDOW),
            max_output_tokens: muse_spark
                .then_some(crate::constants::META_MUSE_SPARK_MAX_OUTPUT_TOKENS),
            emits_provider_continuation: true,
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

    async fn start_response(
        &self,
        request: &ChatRequest,
        ctx: &StreamContext,
    ) -> Result<reqwest::Response> {
        let url = format!("{}/responses", self.base_url.trim_end_matches('/'));
        let body = build_request_body(request, &self.model_name);
        let mut builder = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .header("Accept", "text/event-stream")
            .json(&body);
        for (name, value) in &self.extra_headers {
            builder = builder.header(name, value);
        }
        let response = tokio::select! {
            biased;
            _ = ctx.token.cancelled() => return Err(ModelError::Cancelled),
            response = builder.send() => response.map_err(|error| {
                ModelError::Backend(BackendError::ConnectionFailed {
                    backend: "meta".to_string(),
                    url,
                    reason: error.to_string(),
                })
            })?,
        };
        if response.status().is_success() {
            Ok(response)
        } else {
            Err(http_error(response, &ctx.token).await)
        }
    }
}

#[async_trait]
impl ModelProvider for MetaProvider {
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    async fn chat(&self, request: ChatRequest, ctx: StreamContext) -> Result<FinalResponse> {
        let response = self.start_response(&request, &ctx).await?;
        let mut stream = response.bytes_stream();
        let mut buffer = Vec::new();
        let mut state = ResponseState::default();

        loop {
            let chunk = tokio::select! {
                biased;
                _ = ctx.token.cancelled() => return Err(ModelError::Cancelled),
                chunk = stream.next() => chunk,
            };
            let Some(chunk) = chunk else {
                return Err(ModelError::StreamError(
                    "Meta Responses stream closed before a terminal event".to_string(),
                ));
            };
            // Bound SSE reassembly like every other streaming adapter: a
            // server that streams bytes but never emits the `\n\n` event
            // separator would otherwise grow `buffer` without bound (#50).
            if buffer.len() > crate::constants::MAX_SSE_BUFFER_BYTES {
                return Err(ModelError::StreamError(format!(
                    "SSE stream exceeded {} byte reassembly cap without a complete event",
                    crate::constants::MAX_SSE_BUFFER_BYTES
                )));
            }
            buffer.extend_from_slice(&chunk.map_err(|error| {
                ModelError::StreamError(format!("Meta Responses stream failed: {error}"))
            })?);

            for payload in drain_sse_events(&mut buffer) {
                let event: Value =
                    serde_json::from_str(&payload).map_err(|error| ModelError::ParseError {
                        message: format!("failed to parse Meta Responses event: {error}"),
                        raw: None,
                    })?;
                if let Some(final_response) = handle_event(event, &ctx, &mut state).await? {
                    return Ok(final_response);
                }
            }
        }
    }
}

#[derive(Default)]
struct ResponseState {
    emitted_calls: HashSet<String>,
    tool_calls: Vec<ToolCall>,
}

async fn handle_event(
    event: Value,
    ctx: &StreamContext,
    state: &mut ResponseState,
) -> Result<Option<FinalResponse>> {
    let event_type = event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match event_type {
        "response.output_text.delta" => {
            if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                send(&ctx.sink, StreamEvent::Text(delta.to_string())).await?;
            }
        },
        "response.reasoning_summary_text.delta" => {
            if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                send(
                    &ctx.sink,
                    StreamEvent::Reasoning(ReasoningChunk {
                        text: delta.to_string(),
                        signature: None,
                    }),
                )
                .await?;
            }
        },
        "response.output_item.done" => {
            if let Some(item) = event.get("item") {
                emit_tool_call(item, ctx, state).await?;
            }
        },
        "response.completed" | "response.incomplete" => {
            let response = event
                .get("response")
                .ok_or_else(|| ModelError::ParseError {
                    message: format!("Meta {event_type} event omitted response"),
                    raw: None,
                })?;
            return terminal_response(response, event_type, ctx, state)
                .await
                .map(Some);
        },
        "response.failed" | "error" => return Err(meta_failure(&event)),
        "response.cancelled" => {
            return Err(ModelError::StreamError(
                "Meta cancelled the response".to_string(),
            ));
        },
        _ => {},
    }
    Ok(None)
}

async fn terminal_response(
    response: &Value,
    event_type: &str,
    ctx: &StreamContext,
    state: &mut ResponseState,
) -> Result<FinalResponse> {
    let output = response
        .get("output")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for item in &output {
        emit_tool_call(item, ctx, state).await?;
    }
    let continuation = ProviderContinuation::MetaResponses {
        output: output
            .into_iter()
            .filter(meta_item_is_replayable)
            .map(MetaResponseItem::from_wire)
            .collect(),
    };
    let usage = response.get("usage").map(meta_usage);
    let stop_reason = meta_finish_reason(response, event_type, !state.tool_calls.is_empty());
    send(
        &ctx.sink,
        StreamEvent::Done {
            usage: usage.clone(),
            provider_continuation: Some(continuation.clone()),
            stop_reason: Some(stop_reason.clone()),
        },
    )
    .await?;
    Ok(FinalResponse {
        usage,
        provider_continuation: Some(continuation),
        tool_calls: state.tool_calls.clone(),
        stop_reason: Some(stop_reason),
    })
}

async fn emit_tool_call(
    item: &Value,
    ctx: &StreamContext,
    state: &mut ResponseState,
) -> Result<()> {
    let Some(call) = tool_call_from_item(item) else {
        return Ok(());
    };
    let call_id = call.id.clone().unwrap_or_default();
    if state.emitted_calls.insert(call_id) {
        state.tool_calls.push(call.clone());
        send(&ctx.sink, StreamEvent::ToolCall(call)).await?;
    }
    Ok(())
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
    let arguments = serde_json::from_str(raw_arguments)
        .unwrap_or_else(|_| Value::String(raw_arguments.to_string()));
    Some(ToolCall {
        id: Some(call_id),
        function: FunctionCall { name, arguments },
    })
}

fn build_request_body(request: &ChatRequest, model_name: &str) -> Value {
    let effort = nearest_effort(request.reasoning, &meta_reasoning_levels())
        .unwrap_or(ReasoningLevel::Minimal);
    let mut body = json!({
        "model": model_name,
        "input": messages_to_input(&request.messages),
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
    if (request.temperature - crate::constants::DEFAULT_TEMPERATURE).abs() > f32::EPSILON {
        body["temperature"] = json!(request.temperature);
    }
    let instructions = combined_instructions(request);
    if !instructions.is_empty() {
        body["instructions"] = Value::String(instructions);
    }
    if !request.tools.is_empty() {
        body["tools"] = Value::Array(
            request
                .tools
                .iter()
                .map(|tool| {
                    json!({
                        "type": "function",
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.input_schema,
                    })
                })
                .collect(),
        );
    }
    if request.max_tokens > 0 {
        let limit = request
            .resolved_max_output
            .map_or(request.max_tokens, |max| request.max_tokens.min(max));
        body["max_output_tokens"] = json!(limit);
    }
    body
}

fn messages_to_input(messages: &[crate::models::ChatMessage]) -> Vec<Value> {
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

fn input_message(message: &crate::models::ChatMessage, role: &str, text_type: &str) -> Value {
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

fn combined_instructions(request: &ChatRequest) -> String {
    match request
        .instructions
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        Some(suffix) if !request.system_prompt.is_empty() => {
            format!("{}\n\n{}", request.system_prompt, suffix)
        },
        Some(suffix) => suffix.to_string(),
        None => request.system_prompt.clone(),
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
        debug: crate::models::ResponseDebugContext::default(),
    })
}

async fn http_error(
    response: reqwest::Response,
    token: &tokio_util::sync::CancellationToken,
) -> ModelError {
    let status = response.status().as_u16();
    let debug = crate::models::ResponseDebugContext::from_headers(response.headers());
    let body = tokio::select! {
        biased;
        _ = token.cancelled() => return ModelError::Cancelled,
        body = response.text() => body.unwrap_or_else(|_| "Meta request failed".to_string()),
    };
    ModelError::Backend(BackendError::HttpError {
        status,
        message: crate::utils::redact_secrets(&body),
        debug,
    })
}

async fn send(sink: &tokio::sync::mpsc::Sender<StreamEvent>, event: StreamEvent) -> Result<()> {
    sink.send(event)
        .await
        .map_err(|_| ModelError::StreamError("stream receiver closed".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ToolDefinition, TurnId};
    use crate::models::ChatMessage;
    use crate::providers::test_stream_context;

    fn request() -> ChatRequest {
        ChatRequest {
            model_id: "meta/muse-spark-1.1".to_string(),
            messages: vec![ChatMessage::user("hello").with_images(vec!["PNG".to_string()])],
            system_prompt: "system".to_string(),
            instructions: Some("project".to_string()),
            reasoning: ReasoningLevel::Max,
            temperature: 0.7,
            max_tokens: 200_000,
            tools: vec![ToolDefinition {
                name: "read_file".to_string(),
                description: "Read a file".to_string(),
                input_schema: json!({"type": "object"}),
            }],
            ollama_num_ctx: None,
            ollama_allow_ram_offload: None,
            resolved_context_window: Some(crate::constants::META_MUSE_SPARK_CONTEXT_WINDOW),
            resolved_max_output: Some(crate::constants::META_MUSE_SPARK_MAX_OUTPUT_TOKENS),
            output_schema: None,
            suppress_auto_compact: false,
            suppressed_builtin_tools: Vec::new(),
        }
    }

    #[test]
    fn request_uses_stateless_encrypted_replay_shape() {
        let body = build_request_body(&request(), "muse-spark-1.1");
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

    /// Adapter contract (see `MessageAudience`): harness steering must reach
    /// the model. The Responses API carries system-role input messages, so the
    /// reminder passes through in place at the tail.
    #[test]
    fn model_directed_system_messages_reach_the_wire_in_place() {
        use crate::models::ChatMessageKind;
        let mut req = request();
        let mut nudge = crate::models::ChatMessage::system("Reminder: plan mode is active.");
        nudge.kind = ChatMessageKind::RecoveryNudge;
        req.messages.push(nudge);
        let body = build_request_body(&req, "muse-spark-1.1");

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
        let mut req = request();
        req.reasoning = ReasoningLevel::None;
        req.max_tokens = 0;
        let body = build_request_body(&req, "muse-spark-1.1");
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
        .unwrap();
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

    #[tokio::test]
    async fn response_events_stream_in_order_and_emit_one_terminal_event() {
        let (ctx, mut rx) = test_stream_context(TurnId(7));
        let mut state = ResponseState::default();
        assert!(
            handle_event(
                json!({"type": "response.reasoning_summary_text.delta", "delta": "plan"}),
                &ctx,
                &mut state,
            )
            .await
            .unwrap()
            .is_none()
        );
        assert!(
            handle_event(
                json!({"type": "response.output_text.delta", "delta": "checking"}),
                &ctx,
                &mut state,
            )
            .await
            .unwrap()
            .is_none()
        );
        let function_call = json!({
            "type": "function_call",
            "call_id": "call_1",
            "name": "read_file",
            "arguments": "{\"path\":\"README.md\"}",
            "status": "completed"
        });
        handle_event(
            json!({"type": "response.output_item.done", "item": function_call.clone()}),
            &ctx,
            &mut state,
        )
        .await
        .unwrap();
        let final_response = handle_event(
            json!({
                "type": "response.completed",
                "response": {
                    "status": "completed",
                    "output": [
                        {
                            "type": "reasoning",
                            "id": "rs_1",
                            "summary": [],
                            "encrypted_content": "ciphertext"
                        },
                        {
                            "type": "message",
                            "role": "assistant",
                            "phase": "commentary",
                            "content": [{"type": "output_text", "text": "checking"}]
                        },
                        function_call
                    ],
                    "usage": {"input_tokens": 10, "output_tokens": 5, "total_tokens": 15}
                }
            }),
            &ctx,
            &mut state,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(final_response.tool_calls.len(), 1);
        assert!(matches!(
            final_response.provider_continuation,
            Some(ProviderContinuation::MetaResponses { .. })
        ));

        let mut kinds = Vec::new();
        while let Ok(event) = rx.try_recv() {
            kinds.push(match event {
                StreamEvent::Reasoning(_) => "reasoning",
                StreamEvent::Text(_) => "text",
                StreamEvent::ToolCall(_) => "tool",
                StreamEvent::Done { .. } => "done",
                StreamEvent::Status(_) => "status",
            });
        }
        assert_eq!(kinds, vec!["reasoning", "text", "tool", "done"]);
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
}
