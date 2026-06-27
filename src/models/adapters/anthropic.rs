//! Anthropic Claude adapter — bespoke handling for the Messages API.
//!
//! Anthropic's wire format is structurally different from OpenAI's Chat
//! Completions in ways that prevent base-URL reuse: a top-level `system`
//! field instead of a system message, strict alternating roles, no
//! `tool` role (tool results are content blocks inside a user message),
//! flat tool definitions, and typed SSE streaming events. This adapter
//! handles the translation in one focused file.
//!
//! Critical detail: thinking blocks carry an encrypted `signature` that
//! MUST round-trip in conversation history when extended thinking is
//! enabled. Mermaid's `ChatMessage::thinking_signature` field (Step 3
//! Wave 1) holds it across turns. The signature is per-thinking-block
//! server state — drop it and the API returns 400 invalid_request_error
//! claiming reasoning continuity is broken.
//!
//! Streaming uses standard SSE framing (reused from Step 2's
//! `drain_sse_events`) but emits TYPED events (`message_start`,
//! `content_block_start`, `content_block_delta`, etc.) rather than
//! OpenAI's flat delta-shape. Wave 3 implements the state machine.

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::constants::MAX_RESPONSE_CHARS;
use crate::models::ModelCapabilities;
use crate::models::config::ModelConfig;
use crate::models::error::{BackendError, ModelError, Result};
use crate::models::reasoning::{
    ReasoningCapability, ReasoningChunk, ReasoningLevel, nearest_effort,
};
use crate::models::stream::{StreamCallback, StreamEvent};
use crate::models::tool_call::{FunctionCall, ToolCall};
use crate::models::traits::Model;
use crate::models::types::{ChatMessage, FinishReason, MessageRole, ModelResponse, TokenUsage};
use crate::utils::drain_sse_events;

const TRUNCATION_MARKER: &str = "\n\n[TRUNCATED: response exceeded size limit]";
/// API version pin per Anthropic stability guarantee. Bump when a feature
/// we use moves to a newer version line.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Append `chunk` to `buf`, char-boundary-safe truncation at `cap` bytes.
/// Sets `*truncated` once tripped; subsequent calls become no-ops. Same
/// shape as the helpers in the Ollama and OpenAI-compat adapters.
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

/// Append a streaming tool-argument fragment, hard-capping the buffer at
/// `MAX_TOOL_ARG_BYTES`. A crafted stream could otherwise send unbounded
/// `partial_json` fragments and grow this buffer without limit (the daemon is
/// long-lived). Past the cap we stop appending at a char boundary; the
/// now-truncated JSON simply fails to parse and falls back to a raw string —
/// bounded, not an OOM (#14).
fn push_tool_arg(buf: &mut String, frag: &str) {
    let cap = crate::constants::MAX_TOOL_ARG_BYTES;
    if buf.len() >= cap {
        return;
    }
    if buf.len() + frag.len() <= cap {
        buf.push_str(frag);
    } else {
        let room = cap - buf.len();
        let end = frag.floor_char_boundary(room);
        buf.push_str(&frag[..end]);
    }
}

/// Map Anthropic's `stop_reason` onto the normalized [`FinishReason`].
fn map_anthropic_stop_reason(s: &str) -> FinishReason {
    match s {
        "end_turn" | "stop_sequence" => FinishReason::Stop,
        "tool_use" => FinishReason::ToolUse,
        "max_tokens" => FinishReason::Length,
        "refusal" => FinishReason::ContentFilter,
        other => FinishReason::Other(other.to_string()),
    }
}

/// Finalize one completed (or interrupted) content block into the response
/// accumulators. Shared by the `content_block_stop` handler and the post-loop
/// drain, so a block that never received its stop event (a mid-message cutoff)
/// is recovered identically — including a fully-streamed `tool_use`.
fn finalize_block(
    acc: BlockAccumulator,
    text_acc: &mut String,
    thinking_acc: &mut String,
    signature_acc: &mut Option<String>,
    tool_calls_done: &mut Vec<ToolCall>,
    callback: &StreamCallback,
) {
    match acc {
        BlockAccumulator::Text(s) => text_acc.push_str(&s),
        BlockAccumulator::Thinking { content, signature } => {
            thinking_acc.push_str(&content);
            if signature.is_some() {
                *signature_acc = signature;
            }
        },
        BlockAccumulator::ToolUse {
            id,
            name,
            input_buf,
        } => {
            let arguments: Value = if input_buf.is_empty() {
                json!({})
            } else {
                match serde_json::from_str(&input_buf) {
                    Ok(v) => v,
                    Err(_) => Value::String(input_buf),
                }
            };
            let tc = ToolCall {
                id: if id.is_empty() { None } else { Some(id) },
                function: FunctionCall { name, arguments },
            };
            callback(StreamEvent::ToolCall(tc.clone()));
            tool_calls_done.push(tc);
        },
        BlockAccumulator::Other => {},
    }
}

/// Adaptive (Claude 4.6+) vs legacy (`budget_tokens`) thinking-config shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThinkingFormat {
    /// `thinking: {type: "adaptive"}` + top-level `effort: "low|medium|high|max"`.
    /// Required on Opus 4.7; recommended on Sonnet 4.6 / Opus 4.6.
    Adaptive,
    /// `thinking: {type: "enabled", budget_tokens: N}`. Required on
    /// Sonnet 4.5 / Opus 4.5 / Haiku 4.5.
    Legacy,
}

/// Pick the thinking-config shape this Claude model accepts. Defaults to
/// `Legacy` for unknown models because legacy works on the broader set
/// of models — only Opus 4.7 outright rejects it. If a user picks an
/// unrecognized newer model and gets a 400, the API's error message
/// will tell them to switch formats.
fn thinking_format_for(model: &str) -> ThinkingFormat {
    let m = model.to_lowercase();
    if m.starts_with("claude-opus-4-7")
        || m.starts_with("claude-sonnet-4-6")
        || m.starts_with("claude-opus-4-6")
    {
        ThinkingFormat::Adaptive
    } else {
        ThinkingFormat::Legacy
    }
}

/// Translate `ReasoningLevel` to a legacy `budget_tokens` value, clamped
/// so it never exceeds `max_tokens - 1024` (the API rejects budgets that
/// don't leave headroom for the actual output).
///
/// Budgets climb monotonically with rank so XHigh (between High and Max)
/// gets a between-the-two budget rather than collapsing onto either
/// neighbor. Legacy models don't expose XHigh on-paper, but the value
/// preserves semantic ordering for callers that snap into this path.
fn legacy_budget_for(level: ReasoningLevel, max_tokens: usize) -> Option<u32> {
    let proposed: u32 = match level {
        ReasoningLevel::None => return None,
        ReasoningLevel::Minimal | ReasoningLevel::Low => 2048,
        ReasoningLevel::Medium => 4096,
        ReasoningLevel::High => 16000,
        // Between High (16k) and Max (32k).
        ReasoningLevel::XHigh => 24000,
        ReasoningLevel::Max => 32000,
    };
    let ceiling = max_tokens.saturating_sub(1024) as u32;
    Some(proposed.min(ceiling).max(1024))
}

/// Models that accept `effort: "max"` per the April 2026 effort-doc
/// table: Mythos Preview, Opus 4.7, Opus 4.6, Sonnet 4.6. For the 4.5
/// family (Sonnet 4.5, Opus 4.5, Haiku 4.5) `max` is not in the accepted
/// set and may 400 — we snap to `"high"` instead. Matching is
/// case-insensitive and prefix-based.
fn supports_max_effort(model: &str) -> bool {
    let m = model.to_lowercase();
    m.starts_with("claude-opus-4-7")
        || m.starts_with("claude-opus-4-6")
        || m.starts_with("claude-sonnet-4-6")
        || m.starts_with("claude-mythos")
}

/// Models that accept `effort: "xhigh"` — Opus 4.7 only per the docs.
/// Sending `xhigh` anywhere else would 400.
fn supports_xhigh_effort(model: &str) -> bool {
    model.to_lowercase().starts_with("claude-opus-4-7")
}

/// Translate `ReasoningLevel` to Anthropic's `effort` string. The
/// `effort` parameter applies to ALL Anthropic models — it's a separate
/// knob from the `thinking` field per the official docs:
/// `platform.claude.com/docs/en/build-with-claude/effort` "The effort
/// parameter is supported by Claude Mythos Preview, Claude Opus 4.7,
/// Claude Opus 4.6, Claude Sonnet 4.6, and Claude Opus 4.5." It shapes
/// overall token spend including text + tool calls (not just thinking).
///
/// Per-tier model gating:
/// - `"xhigh"` is Opus 4.7 only — send elsewhere and you get a 400.
/// - `"max"` accepted by Mythos Preview, Opus 4.7, Opus 4.6, Sonnet 4.6.
///   The 4.5 family (Sonnet 4.5, Opus 4.5, Haiku 4.5) snaps `Max` to
///   `"high"` to avoid a potential 400 from the effort endpoint.
///
/// `XHigh` sits BETWEEN `High` and `Max` semantically — when a model
/// doesn't support `xhigh` verbatim we snap DOWN to `"high"`, never UP
/// to `"max"`. The user picked something below max; delivering max
/// would over-spend their intent.
fn adaptive_effort_for(level: ReasoningLevel, model: &str) -> Option<&'static str> {
    match level {
        ReasoningLevel::None => None,
        ReasoningLevel::Minimal | ReasoningLevel::Low => Some("low"),
        ReasoningLevel::Medium => Some("medium"),
        ReasoningLevel::High => Some("high"),
        ReasoningLevel::XHigh => {
            if supports_xhigh_effort(model) {
                Some("xhigh")
            } else {
                // Snap DOWN to high (XHigh < Max in our enum). Even on
                // models that support `max`, we don't upgrade — the user
                // explicitly picked the between-tier.
                Some("high")
            }
        },
        ReasoningLevel::Max => {
            if supports_max_effort(model) {
                Some("max")
            } else {
                // 4.5-family gate: `max` isn't in the effort table for
                // these models. Snap to the highest tier they do
                // support.
                Some("high")
            }
        },
    }
}

/// Convert Mermaid's OpenAI-shaped tool definitions to Anthropic's flat
/// shape. The translation is mechanical: drop the `{type: "function",
/// function: {...}}` wrapper, rename `parameters` → `input_schema`,
/// add `type: "custom"` so the API can disambiguate from server-managed
/// tool types (`web_search`, `code_interpreter`, `computer_use`). The
/// `type: "custom"` field is documented in the official SDK examples;
/// the API also accepts omission, but explicit is forward-compatible.
fn to_anthropic_tools(openai_tools: &[&Value]) -> Vec<Value> {
    openai_tools
        .iter()
        .filter_map(|tool| {
            let function = tool.get("function")?;
            let name = function.get("name")?.as_str()?;
            let description = function
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("");
            let input_schema = function.get("parameters").cloned().unwrap_or(json!({
                "type": "object",
                "properties": {}
            }));
            Some(json!({
                "type": "custom",
                "name": name,
                "description": description,
                "input_schema": input_schema,
            }))
        })
        .collect()
}

/// Translate Mermaid's `ChatMessage` history into Anthropic's
/// `(system, messages)` shape. The system prompt comes from
/// `ModelConfig::system_prompt`; `MessageRole::System` messages in the
/// history are dropped (they're TUI affordances, not model input).
///
/// Consecutive `MessageRole::Tool` messages are merged into a single
/// user-role message with multiple `tool_result` content blocks because
/// Anthropic forbids consecutive same-role messages and tool results
/// always render as user-role.
///
/// Assistant messages with `thinking + thinking_signature` emit a
/// `thinking` content block paired with the text/tool_use blocks; the
/// signature round-trips so subsequent turns don't 400.
fn convert_messages(messages: &[ChatMessage]) -> (Option<String>, Vec<Value>) {
    let mut system: Option<String> = None;
    let mut out: Vec<Value> = Vec::new();

    let mut i = 0;
    while i < messages.len() {
        let msg = &messages[i];
        match msg.role {
            MessageRole::System => {
                // Use the FIRST system message as the top-level system
                // value. Subsequent system messages (rare) are dropped.
                if system.is_none() {
                    system = Some(msg.content.clone());
                }
                i += 1;
            },
            MessageRole::User => {
                let mut content_blocks: Vec<Value> = Vec::new();
                if !msg.content.is_empty() {
                    content_blocks.push(json!({
                        "type": "text",
                        "text": msg.content,
                    }));
                }
                // Vision: convert each base64 image to an image block.
                if let Some(ref images) = msg.images {
                    for data in images {
                        // Default media type is png — matches Mermaid's
                        // clipboard module output. Unsupported formats
                        // surface a clear 415 from the API.
                        content_blocks.push(json!({
                            "type": "image",
                            "source": {
                                "type": "base64",
                                "media_type": "image/png",
                                "data": data,
                            },
                        }));
                    }
                }
                let content = if content_blocks.len() == 1 && content_blocks[0]["type"] == "text" {
                    // Optimization: a single text block can serialize as
                    // a string (Anthropic accepts both shapes; string is
                    // shorter on the wire).
                    content_blocks[0]["text"].clone()
                } else if content_blocks.is_empty() {
                    // Empty content — emit an empty string (Anthropic
                    // requires non-empty messages, but a downstream 400
                    // is the right signal here).
                    json!("")
                } else {
                    json!(content_blocks)
                };
                out.push(json!({"role": "user", "content": content}));
                i += 1;
            },
            MessageRole::Assistant => {
                let mut content_blocks: Vec<Value> = Vec::new();
                // Thinking block FIRST per Anthropic ordering rules — but ONLY
                // when we also have its signature. The API rejects a
                // signature-less thinking block in history with a 400
                // invalid_request_error, which would break the agent loop. If
                // the signature is missing (failed to persist, migrated row,
                // etc.) drop the thinking trace: the text/tool_use blocks alone
                // are a valid assistant turn.
                if let (Some(thinking), Some(sig)) = (&msg.thinking, &msg.thinking_signature)
                    && !thinking.is_empty()
                    && !sig.is_empty()
                {
                    content_blocks.push(json!({
                        "type": "thinking",
                        "thinking": thinking,
                        "signature": sig,
                    }));
                } else if msg.thinking.as_deref().is_some_and(|t| !t.is_empty()) {
                    tracing::debug!(
                        "dropping assistant thinking block that lacks a signature (would 400)",
                    );
                }
                if !msg.content.is_empty() {
                    content_blocks.push(json!({
                        "type": "text",
                        "text": msg.content,
                    }));
                }
                if let Some(ref tool_calls) = msg.tool_calls {
                    for tc in tool_calls {
                        content_blocks.push(json!({
                            "type": "tool_use",
                            "id": tc.id.clone().unwrap_or_default(),
                            "name": tc.function.name,
                            "input": tc.function.arguments,
                        }));
                    }
                }
                if content_blocks.is_empty() {
                    // Skip empty assistant messages — an artifact of
                    // tool-only responses where content is "" and there
                    // were no tool_calls. Anthropic rejects empty
                    // assistant turns.
                    i += 1;
                    continue;
                }
                out.push(json!({"role": "assistant", "content": content_blocks}));
                i += 1;
            },
            MessageRole::Tool => {
                // Merge consecutive Tool messages into one user-role
                // message containing multiple tool_result blocks.
                let mut tool_blocks: Vec<Value> = Vec::new();
                while i < messages.len() && messages[i].role == MessageRole::Tool {
                    let t = &messages[i];
                    let tool_use_id = t.tool_call_id.clone().unwrap_or_default();
                    tool_blocks.push(json!({
                        "type": "tool_result",
                        "tool_use_id": tool_use_id,
                        "content": t.content,
                    }));
                    i += 1;
                }
                out.push(json!({"role": "user", "content": tool_blocks}));
            },
        }
    }

    (system, out)
}

/// Anthropic Claude adapter.
pub struct AnthropicAdapter {
    client: Client,
    api_key: String,
    base_url: String,
    model_name: String,
    capabilities: ModelCapabilities,
}

impl AnthropicAdapter {
    /// Create a new adapter. `api_key` is already resolved (caller uses
    /// `crate::utils::resolve_api_key`).
    pub fn new(api_key: String, model_name: String, base_url: String) -> Result<Self> {
        let client = Client::builder()
            .pool_max_idle_per_host(10)
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(60))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| {
                ModelError::Backend(BackendError::ConnectionFailed {
                    backend: "anthropic".to_string(),
                    url: base_url.clone(),
                    reason: e.to_string(),
                })
            })?;

        // All current Claude models (Sonnet 4.5+, Opus 4.5+, Haiku 4.5)
        // support extended thinking with reasoning levels. The TUI maps
        // `ReasoningLevel` onto the adapter's chosen format (adaptive vs
        // legacy `budget_tokens`) inside `build_request_body`. `XHigh` is
        // advertised on-paper; `adaptive_effort_for` snaps it to `max` or
        // `high` based on the specific model (Opus 4.7 is the only model
        // that accepts `xhigh` verbatim).
        let capabilities = ModelCapabilities {
            supports_tools: true,
            supports_vision: true,
            supports_reasoning: ReasoningCapability::Levels(vec![
                ReasoningLevel::None,
                ReasoningLevel::Low,
                ReasoningLevel::Medium,
                ReasoningLevel::High,
                ReasoningLevel::Max,
                ReasoningLevel::XHigh,
            ]),
            max_context_tokens: None,
        };

        Ok(Self {
            client,
            api_key,
            base_url,
            model_name,
            capabilities,
        })
    }

    /// Build the JSON request body for `POST /v1/messages`.
    fn build_request_body(&self, messages: &[ChatMessage], config: &ModelConfig) -> Value {
        let (system_from_msgs, anthropic_messages) = convert_messages(messages);
        // ModelConfig.system_prompt wins over any system message in the
        // history (matches the OpenAICompatAdapter pattern). Falls back
        // to whatever convert_messages found.
        let system = config.system_prompt.clone().or(system_from_msgs);

        let mut body = json!({
            "model": self.model_name,
            "messages": anthropic_messages,
            "max_tokens": if config.max_tokens > 0 { config.max_tokens } else { 4096 },
            "stream": true,
        });

        // System prompt: emit as a typed-block array with a
        // `cache_control: ephemeral` marker so Anthropic caches the
        // system prompt across requests (Step 5b). Anthropic's caching
        // gives ~90% input-cost reduction + ~2x latency improvement on
        // cache hits, with a 1,024-token minimum that Mermaid's ~1.6k
        // system prompt easily clears. The flat-string shape is also
        // accepted but doesn't get cached.
        // Step 5h: emit one or two typed-text blocks. Block 1 is the
        // static base prompt (cached forever); block 2, when present,
        // is MERMAID.md content (cached per-project, invalidates on
        // file edit). Two cache_control markers means switching
        // projects invalidates only the dynamic block — the static
        // base stays cached across all your projects.
        if let Some(s) = system
            && !s.is_empty()
        {
            let mut blocks = vec![json!({
                "type": "text",
                "text": s,
                "cache_control": {"type": "ephemeral"},
            })];
            if let Some(suffix) = config.dynamic_system_suffix.as_deref()
                && !suffix.is_empty()
            {
                blocks.push(json!({
                    "type": "text",
                    "text": suffix,
                    "cache_control": {"type": "ephemeral"},
                }));
            }
            body["system"] = json!(blocks);
        }

        // Temperature: Anthropic accepts 0.0..=1.0 (NOT 0..=2 like OpenAI).
        // Clamp defensively so a user with `temperature = 1.5` in their
        // config doesn't get a 400.
        let temp = config.temperature.clamp(0.0, 1.0);
        body["temperature"] = json!(temp);

        // Tools come from `config.tools` (OpenAI-compat shape,
        // populated by the provider wrapper). Translate to Anthropic
        // `type: "custom"` entries; drop web tools when no cloud key
        // is available.
        let no_cloud_key = crate::ollama::get_cloud_api_key().is_none();
        let filtered: Vec<&Value> = config
            .tools
            .iter()
            .filter(|t| {
                let name = t
                    .pointer("/function/name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("");
                !(no_cloud_key && (name == "web_search" || name == "web_fetch"))
            })
            .collect();
        let mut anthropic_tools = to_anthropic_tools(&filtered);
        if !anthropic_tools.is_empty() {
            // Mark the LAST tool with `cache_control: ephemeral` (Step
            // 5b). Anthropic caches everything BEFORE the marker too, so
            // a single marker on the last tool covers all tools + the
            // system prompt above (one big cache breakpoint instead of
            // multiple — there's a hard limit of 4 per request).
            if let Some(last) = anthropic_tools.last_mut()
                && let Some(obj) = last.as_object_mut()
            {
                obj.insert("cache_control".to_string(), json!({"type": "ephemeral"}));
            }
            body["tools"] = json!(anthropic_tools);
        }

        // Reasoning depth: snap onto supported levels first (defensive —
        // current capabilities advertise the full enum, but a future
        // model-specific shrink lands cleanly through this path).
        let effective_reasoning = match &self.capabilities.supports_reasoning {
            ReasoningCapability::Levels(supported) => {
                nearest_effort(config.reasoning, supported).unwrap_or(ReasoningLevel::None)
            },
            _ => config.reasoning,
        };

        // Effort: applies to ALL Anthropic models — it's a separate,
        // broader knob from `thinking` that shapes overall token spend
        // including text + tool calls. Lives at `output_config.effort`,
        // NOT top-level. (We were sending it top-level prior to Step
        // 5c — silently ignored by the API, the model defaulted to
        // `high`. Bug fix.)
        if let Some(effort) = adaptive_effort_for(effective_reasoning, &self.model_name) {
            body["output_config"] = json!({"effort": effort});
        }

        // Thinking format: per-model dispatch.
        match thinking_format_for(&self.model_name) {
            ThinkingFormat::Adaptive => {
                // For adaptive, only emit `thinking` when the user
                // actually wants thinking — adaptive models accept
                // omission as disabled. Bundle the `display` field so
                // Opus 4.7 surfaces reasoning chunks (it defaults to
                // `"omitted"` — would otherwise hide the trace). The
                // `hide_reasoning_trace` flag wires it: `omitted` for
                // hidden, `summarized` for visible.
                if effective_reasoning != ReasoningLevel::None {
                    let display = if config.hide_reasoning_trace {
                        "omitted"
                    } else {
                        "summarized"
                    };
                    body["thinking"] = json!({
                        "type": "adaptive",
                        "display": display,
                    });
                }
            },
            ThinkingFormat::Legacy => {
                if let Some(budget) = legacy_budget_for(
                    effective_reasoning,
                    body["max_tokens"].as_u64().unwrap_or(4096) as usize,
                ) {
                    body["thinking"] = json!({
                        "type": "enabled",
                        "budget_tokens": budget,
                    });
                }
            },
        }

        body
    }

    /// POST `/v1/messages` and return the raw response.
    /// Transparently retries on 5xx, 429, or reqwest connect failures
    /// via `crate::effect::retry_transient_http`.
    async fn send_chat(&self, body: &Value) -> Result<reqwest::Response> {
        let url = format!("{}/messages", self.base_url.trim_end_matches('/'));
        crate::effect::retry_transient_http(|| async {
            self.client
                .post(&url)
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", ANTHROPIC_VERSION)
                .header("content-type", "application/json")
                .json(body)
                .send()
                .await
                .map_err(|e| {
                    ModelError::Backend(BackendError::ConnectionFailed {
                        backend: "anthropic".to_string(),
                        url: url.clone(),
                        reason: e.to_string(),
                    })
                })
        })
        .await
    }

    /// Decode a single non-streaming response into `ModelResponse`.
    /// Anthropic doesn't actually have a non-streaming path the way
    /// OpenAI does — even non-stream requests return a Messages object
    /// directly, not chunked. We use this when the caller passes no
    /// stream callback.
    async fn decode_non_streaming(&self, response: reqwest::Response) -> Result<ModelResponse> {
        if !response.status().is_success() {
            return Err(http_error_from_response(response).await);
        }

        let json: AnthropicResponse =
            response.json().await.map_err(|e| ModelError::ParseError {
                message: format!("Failed to parse Anthropic response: {}", e),
                raw: None,
            })?;

        let mut text_acc = String::new();
        let mut thinking_acc = String::new();
        let mut signature: Option<String> = None;
        let mut tool_calls: Vec<ToolCall> = Vec::new();

        for block in json.content {
            match block {
                ContentBlockOut::Text { text } => text_acc.push_str(&text),
                ContentBlockOut::Thinking {
                    thinking,
                    signature: sig,
                } => {
                    thinking_acc.push_str(&thinking);
                    if sig.is_some() {
                        signature = sig;
                    }
                },
                ContentBlockOut::ToolUse { id, name, input } => {
                    tool_calls.push(ToolCall {
                        id: Some(id),
                        function: FunctionCall {
                            name,
                            arguments: input,
                        },
                    });
                },
                ContentBlockOut::Other => {},
            }
        }

        let prompt_tokens = json.usage.input_tokens.unwrap_or(0);
        let completion_tokens = json.usage.output_tokens.unwrap_or(0);
        let cache_creation = json.usage.cache_creation_input_tokens.unwrap_or(0);
        let cache_read = json.usage.cache_read_input_tokens.unwrap_or(0);
        let usage = TokenUsage::provider(
            prompt_tokens,
            completion_tokens,
            prompt_tokens
                .saturating_add(completion_tokens)
                .saturating_add(cache_creation)
                .saturating_add(cache_read),
        )
        .with_cache_creation(cache_creation)
        .with_cached_input(cache_read);

        let stop_reason = json.stop_reason.as_deref().map(map_anthropic_stop_reason);
        if text_acc.is_empty()
            && tool_calls.is_empty()
            && stop_reason == Some(FinishReason::ContentFilter)
        {
            return Err(ModelError::Backend(BackendError::ProviderError {
                provider: "anthropic".to_string(),
                code: Some("refusal".to_string()),
                message: "Anthropic returned no content (refusal / content filter)".to_string(),
            }));
        }

        Ok(ModelResponse {
            content: text_acc,
            usage: Some(usage),
            model_name: self.model_name.clone(),
            stop_reason,
            thinking: if thinking_acc.is_empty() {
                None
            } else {
                Some(thinking_acc)
            },
            tool_calls: if tool_calls.is_empty() {
                None
            } else {
                Some(tool_calls)
            },
            thinking_signature: signature,
        })
    }

    /// Stream the response, emit typed events, return the final
    /// `ModelResponse`. Wave 3 implementation.
    async fn handle_stream(
        &self,
        response: reqwest::Response,
        callback: StreamCallback,
        hide_reasoning_trace: bool,
    ) -> Result<ModelResponse> {
        if !response.status().is_success() {
            return Err(http_error_from_response(response).await);
        }

        let mut stream = response.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();

        let mut text_acc = String::new();
        let mut thinking_acc = String::new();
        let mut signature_acc: Option<String> = None;
        let mut tool_calls_done: Vec<ToolCall> = Vec::new();
        let mut truncated = false;
        let mut prompt_tokens: usize = 0;
        let mut completion_tokens: usize = 0;
        let mut cache_creation_tokens: usize = 0;
        let mut cache_read_tokens: usize = 0;
        let mut stop_reason: Option<FinishReason> = None;
        // Per-block-index accumulators. Anthropic emits content_block_*
        // events tagged with an `index` field; multiple blocks (text +
        // thinking + tool_use) interleave, so we track each by index.
        let mut blocks: HashMap<usize, BlockAccumulator> = HashMap::new();

        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result.map_err(|e| ModelError::StreamError(e.to_string()))?;
            // Bound SSE reassembly: a server that streams bytes but never emits
            // the `\n\n` event separator would otherwise grow `buf` without
            // bound. At this point `buf` holds only the un-terminated residue
            // from the previous drain, so this never trips on legitimately
            // buffered complete events (#50).
            if buf.len() > crate::constants::MAX_SSE_BUFFER_BYTES {
                return Err(ModelError::StreamError(format!(
                    "SSE stream exceeded {} byte reassembly cap without a complete event",
                    crate::constants::MAX_SSE_BUFFER_BYTES
                )));
            }
            buf.extend_from_slice(&chunk);

            for payload in drain_sse_events(&mut buf) {
                let parsed: Value = match serde_json::from_str(&payload) {
                    Ok(v) => v,
                    Err(e) => {
                        return Err(ModelError::ParseError {
                            message: format!("Failed to parse Anthropic stream chunk: {}", e),
                            raw: Some(payload),
                        });
                    },
                };
                let event_type = parsed.get("type").and_then(|v| v.as_str()).unwrap_or("");

                match event_type {
                    "message_start" => {
                        if let Some(input) = parsed
                            .pointer("/message/usage/input_tokens")
                            .and_then(|v| v.as_u64())
                        {
                            prompt_tokens = input as usize;
                        }
                        if let Some(cache_creation) = parsed
                            .pointer("/message/usage/cache_creation_input_tokens")
                            .and_then(|v| v.as_u64())
                        {
                            cache_creation_tokens = cache_creation as usize;
                        }
                        if let Some(cache_read) = parsed
                            .pointer("/message/usage/cache_read_input_tokens")
                            .and_then(|v| v.as_u64())
                        {
                            cache_read_tokens = cache_read as usize;
                        }
                    },
                    "content_block_start" => {
                        let index =
                            parsed.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                        let block = parsed.get("content_block");
                        let block_type = block
                            .and_then(|b| b.get("type"))
                            .and_then(|t| t.as_str())
                            .unwrap_or("");
                        let acc = match block_type {
                            "text" => BlockAccumulator::Text(String::new()),
                            "thinking" => BlockAccumulator::Thinking {
                                content: String::new(),
                                signature: None,
                            },
                            "tool_use" => {
                                let id = block
                                    .and_then(|b| b.get("id"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let name = block
                                    .and_then(|b| b.get("name"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                BlockAccumulator::ToolUse {
                                    id,
                                    name,
                                    input_buf: String::new(),
                                }
                            },
                            // Unknown block types (e.g., server-tool
                            // results we don't request) — track as inert.
                            _ => BlockAccumulator::Other,
                        };
                        blocks.insert(index, acc);
                    },
                    "content_block_delta" => {
                        let index =
                            parsed.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                        let delta = parsed.get("delta");
                        let delta_type = delta
                            .and_then(|d| d.get("type"))
                            .and_then(|t| t.as_str())
                            .unwrap_or("");
                        let Some(acc) = blocks.get_mut(&index) else {
                            continue;
                        };
                        match (acc, delta_type) {
                            (BlockAccumulator::Text(buf_s), "text_delta") => {
                                let text = delta
                                    .and_then(|d| d.get("text"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");
                                if !text.is_empty() && !truncated {
                                    callback(StreamEvent::Text(text.to_string()));
                                    push_capped(buf_s, text, &mut truncated, MAX_RESPONSE_CHARS);
                                }
                            },
                            (
                                BlockAccumulator::Thinking { content, signature },
                                "thinking_delta",
                            ) => {
                                let text = delta
                                    .and_then(|d| d.get("thinking"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");
                                if !text.is_empty() && !truncated {
                                    if !hide_reasoning_trace {
                                        // #9: this is intentionally `None` here —
                                        // `signature_delta` arrives AFTER the
                                        // thinking deltas, so streamed reasoning
                                        // chunks can't carry it. The final
                                        // `ModelResponse.thinking_signature`
                                        // (captured at block stop) is correct and
                                        // is what round-trips; streamed chunks are
                                        // display-only.
                                        callback(StreamEvent::Reasoning(ReasoningChunk {
                                            text: text.to_string(),
                                            signature: signature.clone(),
                                        }));
                                    }
                                    push_capped(content, text, &mut truncated, MAX_RESPONSE_CHARS);
                                }
                            },
                            (BlockAccumulator::Thinking { signature, .. }, "signature_delta") => {
                                let sig = delta
                                    .and_then(|d| d.get("signature"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");
                                if !sig.is_empty() {
                                    *signature = Some(sig.to_string());
                                }
                            },
                            (BlockAccumulator::ToolUse { input_buf, .. }, "input_json_delta") => {
                                let frag = delta
                                    .and_then(|d| d.get("partial_json"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");
                                push_tool_arg(input_buf, frag);
                            },
                            _ => {
                                // delta type doesn't match block type
                                // (shouldn't happen per spec). Ignore.
                            },
                        }
                    },
                    "content_block_stop" => {
                        let index =
                            parsed.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                        if let Some(acc) = blocks.remove(&index) {
                            finalize_block(
                                acc,
                                &mut text_acc,
                                &mut thinking_acc,
                                &mut signature_acc,
                                &mut tool_calls_done,
                                &callback,
                            );
                        }
                    },
                    "message_delta" => {
                        // Cumulative output tokens — overwrite each time.
                        if let Some(out) = parsed
                            .pointer("/usage/output_tokens")
                            .and_then(|v| v.as_u64())
                        {
                            completion_tokens = out as usize;
                        }
                        // The terminal stop_reason rides on `message_delta`.
                        if let Some(sr) = parsed
                            .pointer("/delta/stop_reason")
                            .and_then(|v| v.as_str())
                        {
                            stop_reason = Some(map_anthropic_stop_reason(sr));
                        }
                    },
                    "message_stop" => {
                        // Stream complete. The `Done` event is emitted
                        // unconditionally below after the loop.
                        break;
                    },
                    "error" => {
                        let err_type = parsed
                            .pointer("/error/type")
                            .and_then(|v| v.as_str())
                            .unwrap_or("api_error");
                        let err_msg = parsed
                            .pointer("/error/message")
                            .and_then(|v| v.as_str())
                            .unwrap_or("Anthropic stream error");
                        return Err(ModelError::Backend(BackendError::ProviderError {
                            provider: "anthropic".to_string(),
                            code: Some(err_type.to_string()),
                            message: err_msg.to_string(),
                        }));
                    },
                    "ping" | "" => {
                        // Heartbeats and untyped events — ignore.
                    },
                    _ => {
                        // Unknown event type — log via debug, ignore.
                        tracing::debug!("Anthropic: unknown event type: {}", event_type);
                    },
                }
            }
        }

        // The stream may end without a `message_stop` (e.g. a proxy sends
        // `Connection: close` mid-message). Finalize any blocks still open so a
        // fully-streamed `tool_use` or text block isn't silently dropped — the
        // agent would otherwise "forget" the tool call and return an empty Ok.
        if !blocks.is_empty() {
            tracing::warn!(
                open_blocks = blocks.len(),
                "Anthropic stream ended without message_stop; draining open blocks"
            );
            let mut remaining: Vec<(usize, BlockAccumulator)> = blocks.into_iter().collect();
            remaining.sort_by_key(|(idx, _)| *idx);
            for (_idx, acc) in remaining {
                finalize_block(
                    acc,
                    &mut text_acc,
                    &mut thinking_acc,
                    &mut signature_acc,
                    &mut tool_calls_done,
                    &callback,
                );
            }
        }

        let total_tokens = prompt_tokens
            .saturating_add(completion_tokens)
            .saturating_add(cache_creation_tokens)
            .saturating_add(cache_read_tokens);
        // F3: `Done` is emitted by the v0.7 wrapper from the returned
        // `ModelResponse` so the `thinking_signature` round-trips. If we
        // emitted it here, the reducer would commit the assistant
        // message on our signature-less Done and drop the real one.

        // A refusal / content block that produced no usable output is an
        // error, not an empty success (matches the non-streaming path).
        if text_acc.is_empty()
            && tool_calls_done.is_empty()
            && stop_reason == Some(FinishReason::ContentFilter)
        {
            return Err(ModelError::Backend(BackendError::ProviderError {
                provider: "anthropic".to_string(),
                code: Some("refusal".to_string()),
                message: "Anthropic returned no content (refusal / content filter)".to_string(),
            }));
        }

        Ok(ModelResponse {
            content: text_acc,
            usage: Some(
                TokenUsage::provider(prompt_tokens, completion_tokens, total_tokens)
                    .with_cache_creation(cache_creation_tokens)
                    .with_cached_input(cache_read_tokens),
            ),
            model_name: self.model_name.clone(),
            stop_reason,
            thinking: if thinking_acc.is_empty() {
                None
            } else {
                Some(thinking_acc)
            },
            tool_calls: if tool_calls_done.is_empty() {
                None
            } else {
                Some(tool_calls_done)
            },
            thinking_signature: signature_acc,
        })
    }
}

#[async_trait]
impl Model for AnthropicAdapter {
    fn name(&self) -> &str {
        &self.model_name
    }

    fn capabilities(&self) -> &ModelCapabilities {
        &self.capabilities
    }

    /// Anthropic doesn't expose a public model-listing endpoint. Surface
    /// the static fact rather than 404.
    async fn list_models(&self) -> Result<Vec<String>> {
        Err(ModelError::Unsupported {
            feature: "list_models (anthropic)".to_string(),
        })
    }

    async fn chat(
        &self,
        messages: &[ChatMessage],
        config: &ModelConfig,
        callback: Option<StreamCallback>,
    ) -> Result<ModelResponse> {
        let mut body = self.build_request_body(messages, config);
        let stream = callback.is_some();
        if !stream {
            body["stream"] = json!(false);
        }
        let response = self.send_chat(&body).await?;
        if let Some(cb) = callback {
            self.handle_stream(response, cb, config.hide_reasoning_trace)
                .await
        } else {
            self.decode_non_streaming(response).await
        }
    }
}

// ===== Wire types =====

/// Non-streaming response shape (`POST /v1/messages` without `stream`).
#[derive(Debug, Deserialize)]
struct AnthropicResponse {
    content: Vec<ContentBlockOut>,
    #[serde(default)]
    usage: UsageOut,
    #[serde(default)]
    stop_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct UsageOut {
    #[serde(default)]
    input_tokens: Option<usize>,
    #[serde(default)]
    output_tokens: Option<usize>,
    #[serde(default)]
    cache_creation_input_tokens: Option<usize>,
    #[serde(default)]
    cache_read_input_tokens: Option<usize>,
}

/// Output content blocks Anthropic returns (subset we care about).
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlockOut {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
        #[serde(default)]
        signature: Option<String>,
    },
    ToolUse {
        #[allow(dead_code)]
        id: String,
        #[allow(dead_code)]
        name: String,
        #[allow(dead_code)]
        input: Value,
    },
    /// Catch-all for content types we don't model (server-tool results,
    /// future block types). Falls through cleanly via serde's untagged
    /// enum semantics. We use a struct variant rather than `#[serde(other)]`
    /// because the latter only works on unit variants.
    #[serde(other)]
    Other,
}

/// Per-block-index streaming accumulator. Anthropic interleaves
/// content_block events for multiple blocks (text + thinking + tool_use),
/// indexed by `index`. We keep one accumulator per active block.
#[derive(Debug)]
enum BlockAccumulator {
    Text(String),
    Thinking {
        content: String,
        signature: Option<String>,
    },
    ToolUse {
        id: String,
        name: String,
        input_buf: String,
    },
    /// Catch-all for unknown content block types — e.g., server-tool
    /// results we never requested. Ignored on the way in and out.
    Other,
}

/// Translate a non-success HTTP response into a structured `ModelError`.
async fn http_error_from_response(response: reqwest::Response) -> ModelError {
    let status = response.status().as_u16();
    let body = response
        .text()
        .await
        .unwrap_or_else(|_| "Unknown error".to_string());
    // Try to parse Anthropic's error JSON shape so the user sees the
    // actual error message rather than a raw JSON blob.
    if let Ok(parsed) = serde_json::from_str::<Value>(&body)
        && let (Some(err_type), Some(err_msg)) = (
            parsed.pointer("/error/type").and_then(|v| v.as_str()),
            parsed.pointer("/error/message").and_then(|v| v.as_str()),
        )
    {
        // 400 invalid_request_error mentioning thinking is the
        // signature round-trip going wrong — flag it specifically so
        // future debugging starts at the right place.
        if status == 400 && err_msg.to_lowercase().contains("thinking") {
            return ModelError::Backend(BackendError::ProviderError {
                provider: "anthropic".to_string(),
                code: Some(err_type.to_string()),
                message: format!(
                    "{} (thinking-block round-trip failed; this is a Mermaid bug — \
                         please open an issue with the conversation that triggered it)",
                    err_msg
                ),
            });
        }
        return ModelError::Backend(BackendError::ProviderError {
            provider: "anthropic".to_string(),
            code: Some(err_type.to_string()),
            message: err_msg.to_string(),
        });
    }
    ModelError::Backend(BackendError::HttpError {
        status,
        message: body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn has_thinking_block(msgs: &[serde_json::Value]) -> bool {
        msgs.iter().any(|msg| {
            msg.get("content")
                .and_then(|c| c.as_array())
                .map(|blocks| {
                    blocks
                        .iter()
                        .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("thinking"))
                })
                .unwrap_or(false)
        })
    }

    #[test]
    fn maps_anthropic_stop_reasons() {
        assert_eq!(map_anthropic_stop_reason("end_turn"), FinishReason::Stop);
        assert_eq!(
            map_anthropic_stop_reason("max_tokens"),
            FinishReason::Length
        );
        assert_eq!(map_anthropic_stop_reason("tool_use"), FinishReason::ToolUse);
        assert_eq!(
            map_anthropic_stop_reason("refusal"),
            FinishReason::ContentFilter
        );
    }

    #[test]
    fn finalize_block_recovers_tool_use() {
        // #4: a fully-streamed tool_use block must be recovered even when it's
        // drained outside `content_block_stop` (the mid-cutoff path).
        let events: std::sync::Arc<std::sync::Mutex<Vec<StreamEvent>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let ev = events.clone();
        let cb: StreamCallback = std::sync::Arc::new(move |e| ev.lock().unwrap().push(e));
        let mut text = String::new();
        let mut thinking = String::new();
        let mut sig = None;
        let mut tools = Vec::new();
        finalize_block(
            BlockAccumulator::ToolUse {
                id: "tu_1".to_string(),
                name: "read_file".to_string(),
                input_buf: r#"{"path":"a.txt"}"#.to_string(),
            },
            &mut text,
            &mut thinking,
            &mut sig,
            &mut tools,
            &cb,
        );
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].function.name, "read_file");
        assert_eq!(events.lock().unwrap().len(), 1);
    }

    #[test]
    fn thinking_block_requires_signature() {
        // H22: a thinking block without a signature 400s the next request, so
        // it must be dropped; with a signature it must be emitted.
        let mut unsigned = ChatMessage::assistant("answer");
        unsigned.thinking = Some("private reasoning".to_string());
        let (_sys, msgs) = convert_messages(&[unsigned]);
        assert!(
            !has_thinking_block(&msgs),
            "unsigned thinking must be dropped"
        );

        let mut signed = ChatMessage::assistant("answer").with_thinking_signature("sig123");
        signed.thinking = Some("private reasoning".to_string());
        let (_sys, msgs) = convert_messages(&[signed]);
        assert!(has_thinking_block(&msgs), "signed thinking must be present");
    }

    fn test_adapter() -> AnthropicAdapter {
        AnthropicAdapter::new(
            "test-key".to_string(),
            "claude-sonnet-4-6".to_string(),
            "https://api.anthropic.com/v1".to_string(),
        )
        .expect("adapter constructs")
    }

    // --- Helpers ---

    #[test]
    fn thinking_format_dispatch() {
        assert_eq!(
            thinking_format_for("claude-opus-4-7"),
            ThinkingFormat::Adaptive
        );
        assert_eq!(
            thinking_format_for("claude-sonnet-4-6"),
            ThinkingFormat::Adaptive
        );
        assert_eq!(
            thinking_format_for("claude-opus-4-6"),
            ThinkingFormat::Adaptive
        );
        assert_eq!(
            thinking_format_for("claude-sonnet-4-5"),
            ThinkingFormat::Legacy
        );
        assert_eq!(
            thinking_format_for("claude-opus-4-5"),
            ThinkingFormat::Legacy
        );
        assert_eq!(
            thinking_format_for("claude-haiku-4-5"),
            ThinkingFormat::Legacy
        );
        // Case insensitive.
        assert_eq!(
            thinking_format_for("Claude-Opus-4-7-Special"),
            ThinkingFormat::Adaptive
        );
        // Unknown defaults to Legacy.
        assert_eq!(
            thinking_format_for("claude-future-99"),
            ThinkingFormat::Legacy
        );
    }

    #[test]
    fn legacy_budget_clamps_to_max_tokens() {
        // High level normally maps to 16000; with max_tokens=8000 we
        // clamp to 8000 - 1024 = 6976. The result also has a 1024 floor.
        assert_eq!(legacy_budget_for(ReasoningLevel::High, 8000), Some(6976));
        // Low level (2048) fits within max_tokens (4096), no clamp.
        assert_eq!(legacy_budget_for(ReasoningLevel::Low, 4096), Some(2048));
        // None → None.
        assert_eq!(legacy_budget_for(ReasoningLevel::None, 4096), None);
        // Max with generous max_tokens → 32000.
        assert_eq!(legacy_budget_for(ReasoningLevel::Max, 64000), Some(32000));
        // Max with low max_tokens → clamped, but not below 1024.
        assert_eq!(legacy_budget_for(ReasoningLevel::Max, 2000), Some(1024));
    }

    #[test]
    fn adaptive_effort_per_level() {
        let m = "claude-sonnet-4-6";
        assert_eq!(adaptive_effort_for(ReasoningLevel::None, m), None);
        assert_eq!(adaptive_effort_for(ReasoningLevel::Minimal, m), Some("low"));
        assert_eq!(adaptive_effort_for(ReasoningLevel::Low, m), Some("low"));
        assert_eq!(
            adaptive_effort_for(ReasoningLevel::Medium, m),
            Some("medium")
        );
        assert_eq!(adaptive_effort_for(ReasoningLevel::High, m), Some("high"));
        // Sonnet 4.6 supports `max` per the effort-doc table.
        assert_eq!(adaptive_effort_for(ReasoningLevel::Max, m), Some("max"));
    }

    /// Opus 4.7 supports the `xhigh` effort tier (between `high` and
    /// `max` in our enum; Anthropic exposes it as a distinct string on
    /// the wire). Other models would 400 on `xhigh`, so the gate is
    /// Opus 4.7-only.
    #[test]
    fn adaptive_effort_uses_xhigh_on_opus_4_7_for_xhigh() {
        assert_eq!(
            adaptive_effort_for(ReasoningLevel::XHigh, "claude-opus-4-7"),
            Some("xhigh")
        );
        // Opus 4.7 also supports `max` — verify Max still maps to max
        // (distinct tier from xhigh).
        assert_eq!(
            adaptive_effort_for(ReasoningLevel::Max, "claude-opus-4-7"),
            Some("max")
        );
        // XHigh on Opus 4.6 (no xhigh support): XHigh sits between High
        // and Max in our enum, so we snap DOWN to "high" — never up to
        // "max". Upgrading would over-spend the user's explicit choice.
        assert_eq!(
            adaptive_effort_for(ReasoningLevel::XHigh, "claude-opus-4-6"),
            Some("high")
        );
    }

    /// 4.5-family models don't accept `max` per the April 2026 effort
    /// doc table. `Max` must snap to `high` to avoid a 400.
    #[test]
    fn adaptive_effort_gates_max_on_4_5_family() {
        for m in ["claude-sonnet-4-5", "claude-opus-4-5", "claude-haiku-4-5"] {
            assert_eq!(
                adaptive_effort_for(ReasoningLevel::Max, m),
                Some("high"),
                "model {} should snap Max → high (no max effort support)",
                m
            );
            // XHigh on 4.5-family snaps directly to high (neither xhigh
            // nor max is supported).
            assert_eq!(
                adaptive_effort_for(ReasoningLevel::XHigh, m),
                Some("high"),
                "model {} should snap XHigh → high",
                m
            );
        }
    }

    // --- Tool translation ---

    #[test]
    fn tool_translation_drops_function_wrapper() {
        let openai_tool = json!({
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read a file",
                "parameters": {
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"]
                }
            }
        });
        let translated = to_anthropic_tools(&[&openai_tool]);
        assert_eq!(translated.len(), 1);
        assert_eq!(translated[0]["name"], "read_file");
        assert_eq!(translated[0]["description"], "Read a file");
        // Step 5c: `type: "custom"` is added explicitly so the API can
        // disambiguate from server-managed tool types.
        assert_eq!(translated[0]["type"], "custom");
        // The OpenAI `{type: "function", function: {...}}` wrapper is
        // gone — only the inner fields plus `type: "custom"` remain.
        assert!(translated[0].get("function").is_none());
        // `parameters` was renamed to `input_schema`.
        assert_eq!(
            translated[0]["input_schema"]["properties"]["path"]["type"],
            "string"
        );
    }

    #[test]
    fn tool_translation_handles_missing_description() {
        let openai_tool = json!({
            "type": "function",
            "function": {
                "name": "no_description_tool",
                "parameters": {"type": "object", "properties": {}}
            }
        });
        let translated = to_anthropic_tools(&[&openai_tool]);
        assert_eq!(translated[0]["description"], "");
    }

    // --- Message conversion ---

    #[test]
    fn convert_messages_extracts_system_only_first() {
        let messages = vec![
            ChatMessage::system("You are helpful."),
            ChatMessage::user("Hello"),
            ChatMessage::system("This second system message is dropped."),
        ];
        let (system, msgs) = convert_messages(&messages);
        assert_eq!(system.as_deref(), Some("You are helpful."));
        // Only the user message ends up in the messages array.
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
    }

    #[test]
    fn convert_messages_merges_consecutive_tool_messages() {
        // Agent loop produces: assistant(tool_calls) → tool → tool → tool
        // → assistant(text). The three Tool messages must collapse into
        // ONE user-role message with three tool_result blocks so the
        // role-alternation rule isn't violated.
        let messages = vec![
            ChatMessage::user("Read three files"),
            {
                let mut m = ChatMessage::assistant("I will read them.");
                m.tool_calls = Some(vec![
                    ToolCall {
                        id: Some("c1".to_string()),
                        function: FunctionCall {
                            name: "read_file".into(),
                            arguments: json!({"path": "a.txt"}),
                        },
                    },
                    ToolCall {
                        id: Some("c2".to_string()),
                        function: FunctionCall {
                            name: "read_file".into(),
                            arguments: json!({"path": "b.txt"}),
                        },
                    },
                    ToolCall {
                        id: Some("c3".to_string()),
                        function: FunctionCall {
                            name: "read_file".into(),
                            arguments: json!({"path": "c.txt"}),
                        },
                    },
                ]);
                m
            },
            ChatMessage::tool("c1", "read_file", "contents of a"),
            ChatMessage::tool("c2", "read_file", "contents of b"),
            ChatMessage::tool("c3", "read_file", "contents of c"),
            ChatMessage::assistant("Done."),
        ];
        let (_, msgs) = convert_messages(&messages);
        // Sequence after merge: user → assistant(text+tool_use*3) →
        // user(tool_result*3) → assistant(text). 4 messages.
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[2]["role"], "user");
        assert_eq!(msgs[3]["role"], "assistant");
        // The tool-results message is an array of three tool_result blocks.
        let tool_results = msgs[2]["content"].as_array().expect("array");
        assert_eq!(tool_results.len(), 3);
        for (i, expected_id) in ["c1", "c2", "c3"].iter().enumerate() {
            assert_eq!(tool_results[i]["type"], "tool_result");
            assert_eq!(tool_results[i]["tool_use_id"], *expected_id);
        }
    }

    #[test]
    fn convert_messages_emits_thinking_block_with_signature() {
        let mut msg = ChatMessage::assistant("Final answer.");
        msg.thinking = Some("reasoning content".to_string());
        msg.thinking_signature = Some("sig_xyz".to_string());
        let messages = vec![ChatMessage::user("Q?"), msg];
        let (_, msgs) = convert_messages(&messages);
        let assistant_content = msgs[1]["content"].as_array().expect("array");
        // Thinking block first, text block second.
        assert_eq!(assistant_content[0]["type"], "thinking");
        assert_eq!(assistant_content[0]["thinking"], "reasoning content");
        assert_eq!(assistant_content[0]["signature"], "sig_xyz");
        assert_eq!(assistant_content[1]["type"], "text");
        assert_eq!(assistant_content[1]["text"], "Final answer.");
    }

    #[test]
    fn convert_messages_image_block_for_user_with_images() {
        let msg = ChatMessage::user("What is this?").with_images(vec!["BASE64DATA".to_string()]);
        let messages = vec![msg];
        let (_, msgs) = convert_messages(&messages);
        let content = msgs[0]["content"].as_array().expect("array");
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "What is this?");
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["source"]["type"], "base64");
        assert_eq!(content[1]["source"]["media_type"], "image/png");
        assert_eq!(content[1]["source"]["data"], "BASE64DATA");
    }

    // --- Request body ---

    #[test]
    fn build_request_body_includes_required_fields() {
        let adapter = test_adapter();
        let messages = vec![ChatMessage::user("Hello")];
        let config = ModelConfig::default();
        let body = adapter.build_request_body(&messages, &config);
        assert_eq!(body["model"], "claude-sonnet-4-6");
        assert_eq!(body["stream"], true);
        assert!(body["max_tokens"].is_u64());
        assert!(body["messages"].is_array());
    }

    #[test]
    fn build_request_body_sets_system_field_not_message() {
        let adapter = test_adapter();
        let messages = vec![ChatMessage::user("Hi")];
        let config = ModelConfig {
            system_prompt: Some("You are Mermaid.".to_string()),
            ..Default::default()
        };
        let body = adapter.build_request_body(&messages, &config);
        // Step 5b: system serializes as a typed-block array carrying a
        // `cache_control: ephemeral` marker so Anthropic caches it.
        let sys = body["system"].as_array().expect("system is array");
        assert_eq!(sys.len(), 1);
        assert_eq!(sys[0]["type"], "text");
        assert_eq!(sys[0]["text"], "You are Mermaid.");
        assert_eq!(sys[0]["cache_control"]["type"], "ephemeral");
        // System should NOT also appear as a message.
        let msgs = body["messages"].as_array().unwrap();
        for m in msgs {
            assert_ne!(m["role"], "system");
        }
    }

    /// Step 5h: when MERMAID.md content is present, the static base
    /// stays in cache slot #1 and the dynamic suffix gets its own
    /// cache slot #2. Two separately-cached typed-text blocks → static
    /// base survives across project switches; only the suffix re-caches
    /// when the file changes.
    #[test]
    fn build_request_body_emits_two_cache_blocks_when_suffix_present() {
        let adapter = test_adapter();
        let messages = vec![ChatMessage::user("Hi")];
        let config = ModelConfig {
            system_prompt: Some("You are Mermaid.".to_string()),
            dynamic_system_suffix: Some("Project rule: always snake_case.".to_string()),
            ..Default::default()
        };
        let body = adapter.build_request_body(&messages, &config);
        let sys = body["system"].as_array().expect("system is array");
        assert_eq!(sys.len(), 2);
        assert_eq!(sys[0]["text"], "You are Mermaid.");
        assert_eq!(sys[0]["cache_control"]["type"], "ephemeral");
        assert_eq!(sys[1]["text"], "Project rule: always snake_case.");
        assert_eq!(sys[1]["cache_control"]["type"], "ephemeral");
    }

    /// Regression guard: with no dynamic suffix, behavior is byte-equivalent
    /// to pre-Step-5h — single block, single cache marker. Existing sessions
    /// without MERMAID.md must not change cache shape.
    #[test]
    fn build_request_body_emits_single_block_when_suffix_absent() {
        let adapter = test_adapter();
        let messages = vec![ChatMessage::user("Hi")];
        let config = ModelConfig {
            system_prompt: Some("You are Mermaid.".to_string()),
            dynamic_system_suffix: None,
            ..Default::default()
        };
        let body = adapter.build_request_body(&messages, &config);
        let sys = body["system"].as_array().expect("system is array");
        assert_eq!(sys.len(), 1);
        assert_eq!(sys[0]["text"], "You are Mermaid.");
    }

    /// Step 5c bug fix: `effort` lives at `output_config.effort`, NOT
    /// top-level. Adaptive models also need `display: "summarized"` so
    /// Opus 4.7 (which defaults to "omitted") surfaces reasoning chunks.
    #[test]
    fn build_request_body_uses_adaptive_for_sonnet_4_6() {
        let adapter = test_adapter(); // claude-sonnet-4-6
        let messages = vec![ChatMessage::user("Hi")];
        let config = ModelConfig {
            reasoning: ReasoningLevel::High,
            ..Default::default()
        };
        let body = adapter.build_request_body(&messages, &config);
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["thinking"]["display"], "summarized");
        // Effort is in output_config, NOT top-level (Step 5c fix).
        assert_eq!(body["output_config"]["effort"], "high");
        assert!(body.get("effort").is_none(), "effort must NOT be top-level");
        assert!(body["thinking"].get("budget_tokens").is_none());
    }

    /// Step 5c: legacy thinking models (Sonnet 4.5, Opus 4.5, Haiku
    /// 4.5) ALSO get `output_config.effort` set. Effort is a separate
    /// knob from thinking format per the official `effort` doc.
    #[test]
    fn build_request_body_uses_legacy_for_sonnet_4_5() {
        let adapter = AnthropicAdapter::new(
            "k".to_string(),
            "claude-sonnet-4-5".to_string(),
            "https://api.anthropic.com/v1".to_string(),
        )
        .unwrap();
        let messages = vec![ChatMessage::user("Hi")];
        let config = ModelConfig {
            reasoning: ReasoningLevel::Medium,
            max_tokens: 8000,
            ..Default::default()
        };
        let body = adapter.build_request_body(&messages, &config);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 4096);
        // Effort applies to legacy models too (Step 5c fix — was missing
        // before because effort was bundled with the adaptive branch).
        assert_eq!(body["output_config"]["effort"], "medium");
    }

    #[test]
    fn build_request_body_omits_thinking_when_reasoning_is_none() {
        let adapter = test_adapter();
        let messages = vec![ChatMessage::user("Hi")];
        let config = ModelConfig {
            reasoning: ReasoningLevel::None,
            ..Default::default()
        };
        let body = adapter.build_request_body(&messages, &config);
        assert!(body.get("thinking").is_none());
        // None level also means no effort hint (effort defaults to
        // "high" on the API side, which is what we'd want for
        // not-explicitly-controlled requests).
        assert!(body.get("output_config").is_none());
        assert!(body.get("effort").is_none(), "no top-level effort either");
    }

    /// Opus 4.7 + XHigh maps to `xhigh` — the highest tier, available
    /// only on Opus 4.7 per the official docs. Max on Opus 4.7 stays at
    /// `max` (distinct tier from xhigh).
    #[test]
    fn build_request_body_uses_xhigh_on_opus_4_7_for_xhigh() {
        let adapter = AnthropicAdapter::new(
            "k".to_string(),
            "claude-opus-4-7".to_string(),
            "https://api.anthropic.com/v1".to_string(),
        )
        .unwrap();
        let messages = vec![ChatMessage::user("Hi")];
        let config = ModelConfig {
            reasoning: ReasoningLevel::XHigh,
            ..Default::default()
        };
        let body = adapter.build_request_body(&messages, &config);
        assert_eq!(body["output_config"]["effort"], "xhigh");
        assert_eq!(body["thinking"]["type"], "adaptive");
    }

    /// Opus 4.6 + Max maps to `max` (NOT xhigh — that's Opus 4.7-only).
    /// Sending xhigh to Opus 4.6 would 400.
    #[test]
    fn build_request_body_uses_max_on_opus_4_6_for_max() {
        let adapter = AnthropicAdapter::new(
            "k".to_string(),
            "claude-opus-4-6".to_string(),
            "https://api.anthropic.com/v1".to_string(),
        )
        .unwrap();
        let messages = vec![ChatMessage::user("Hi")];
        let config = ModelConfig {
            reasoning: ReasoningLevel::Max,
            ..Default::default()
        };
        let body = adapter.build_request_body(&messages, &config);
        assert_eq!(body["output_config"]["effort"], "max");
    }

    /// 4.5-family models don't accept `max` effort per the docs. The
    /// adapter gate snaps Max (and XHigh) to `high` to avoid a 400.
    #[test]
    fn build_request_body_snaps_max_to_high_on_sonnet_4_5() {
        let adapter = AnthropicAdapter::new(
            "k".to_string(),
            "claude-sonnet-4-5".to_string(),
            "https://api.anthropic.com/v1".to_string(),
        )
        .unwrap();
        let messages = vec![ChatMessage::user("Hi")];
        let config = ModelConfig {
            reasoning: ReasoningLevel::Max,
            max_tokens: 8000,
            ..Default::default()
        };
        let body = adapter.build_request_body(&messages, &config);
        assert_eq!(
            body["output_config"]["effort"], "high",
            "Sonnet 4.5 should snap Max → high (no max effort support)"
        );
    }

    /// Step 5c: `display` defaults to `"summarized"` on adaptive models
    /// so reasoning chunks are visible in the response stream. Without
    /// this, Opus 4.7 users see no reasoning content (it defaults to
    /// `"omitted"` on Opus 4.7 specifically).
    #[test]
    fn build_request_body_sets_display_summarized_by_default() {
        let adapter = test_adapter(); // claude-sonnet-4-6 (adaptive)
        let messages = vec![ChatMessage::user("Hi")];
        let config = ModelConfig {
            reasoning: ReasoningLevel::Medium,
            hide_reasoning_trace: false,
            ..Default::default()
        };
        let body = adapter.build_request_body(&messages, &config);
        assert_eq!(body["thinking"]["display"], "summarized");
    }

    /// Step 5c: when the user enables hide_reasoning_trace, send
    /// `display: "omitted"` so the API doesn't waste bandwidth streaming
    /// thinking tokens we'd just discard client-side.
    #[test]
    fn build_request_body_sets_display_omitted_when_hide_reasoning_trace() {
        let adapter = test_adapter();
        let messages = vec![ChatMessage::user("Hi")];
        let config = ModelConfig {
            reasoning: ReasoningLevel::Medium,
            hide_reasoning_trace: true,
            ..Default::default()
        };
        let body = adapter.build_request_body(&messages, &config);
        assert_eq!(body["thinking"]["display"], "omitted");
    }

    #[test]
    fn build_request_body_clamps_temperature_to_anthropic_range() {
        let adapter = test_adapter();
        let messages = vec![ChatMessage::user("Hi")];
        let config = ModelConfig {
            temperature: 1.5, // OpenAI accepts up to 2.0; Anthropic caps at 1.0
            ..Default::default()
        };
        let body = adapter.build_request_body(&messages, &config);
        assert_eq!(body["temperature"].as_f64().unwrap(), 1.0);
    }

    #[test]
    fn build_request_body_includes_tools_in_anthropic_shape() {
        let adapter = test_adapter();
        let messages = vec![ChatMessage::user("Hi")];
        // Config carries OpenAI-shape tools (populated by the v7
        // provider wrapper from ChatRequest.tools); the adapter
        // translates to Anthropic's flat `type: "custom"` shape.
        let config = ModelConfig {
            tools: vec![serde_json::json!({
                "type": "function",
                "function": {
                    "name": "test_tool",
                    "description": "a test tool",
                    "parameters": {"type": "object", "properties": {}}
                }
            })],
            ..Default::default()
        };
        let body = adapter.build_request_body(&messages, &config);
        let tools = body["tools"].as_array().expect("tools array");
        assert!(!tools.is_empty());
        for tool in tools {
            assert_eq!(tool["type"], "custom");
            assert!(tool.get("function").is_none());
            assert!(tool.get("name").is_some());
            assert!(tool.get("input_schema").is_some());
        }
    }

    /// Step 5b: only the LAST tool gets `cache_control: ephemeral`.
    /// Anthropic caches everything BEFORE the marker too, so a single
    /// marker on the last tool is enough — adding more wastes one of
    /// the 4 cache breakpoints per request.
    #[test]
    fn build_request_body_marks_only_last_tool_with_cache_control() {
        let adapter = test_adapter();
        let messages = vec![ChatMessage::user("Hi")];
        let config = ModelConfig {
            tools: vec![
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": "tool_a",
                        "description": "first",
                        "parameters": {"type": "object"}
                    }
                }),
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": "tool_b",
                        "description": "second",
                        "parameters": {"type": "object"}
                    }
                }),
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": "tool_c",
                        "description": "third",
                        "parameters": {"type": "object"}
                    }
                }),
            ],
            ..Default::default()
        };
        let body = adapter.build_request_body(&messages, &config);
        let tools = body["tools"].as_array().expect("tools array");
        assert!(
            tools.len() >= 2,
            "need at least 2 tools to verify marker placement"
        );

        // All tools except the last must NOT have cache_control.
        for tool in &tools[..tools.len() - 1] {
            assert!(
                tool.get("cache_control").is_none(),
                "non-last tool should not carry cache_control: {:?}",
                tool
            );
        }
        // The last tool MUST have cache_control: ephemeral.
        let last = &tools[tools.len() - 1];
        assert_eq!(
            last["cache_control"]["type"], "ephemeral",
            "last tool should carry the cache_control marker"
        );
    }

    /// When the tool list is empty (no tools registered for this
    /// request), the request body must omit the `tools` field
    /// entirely. No orphan `cache_control` marker on a non-existent
    /// last tool, no panic.
    #[test]
    fn build_request_body_handles_empty_tools_without_panicking() {
        // The translation helper is the right unit-of-test here:
        // if `to_anthropic_tools(&[])` returned a non-empty vec, the
        // adapter's `if !anthropic_tools.is_empty()` guard would let us
        // reach the cache_control insertion with no last element.
        let result = to_anthropic_tools(&[]);
        assert!(result.is_empty(), "empty input must produce empty output");
    }

    #[test]
    fn capabilities_advertise_full_reasoning_levels_and_vision() {
        let adapter = test_adapter();
        let caps = adapter.capabilities();
        assert!(caps.supports_tools);
        assert!(caps.supports_vision);
        match &caps.supports_reasoning {
            ReasoningCapability::Levels(levels) => {
                assert!(levels.contains(&ReasoningLevel::None));
                assert!(levels.contains(&ReasoningLevel::Max));
            },
            other => panic!("expected Levels, got {:?}", other),
        }
    }

    #[test]
    fn name_returns_model_id() {
        let adapter = test_adapter();
        assert_eq!(adapter.name(), "claude-sonnet-4-6");
    }
}
