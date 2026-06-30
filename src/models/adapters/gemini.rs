//! Google Gemini adapter — bespoke handling for the `generateContent` API.
//!
//! Gemini's wire format is structurally different from both OpenAI Chat
//! Completions and Anthropic Messages. Key divergences:
//!
//! - Endpoints are per-method: `POST /models/{model}:generateContent`
//!   (sync) and `POST /models/{model}:streamGenerateContent?alt=sse`
//!   (streaming). The model name lives in the URL, not the body.
//! - Auth is `x-goog-api-key: $KEY` (header) or `?key=` (query). We use
//!   the header — cleaner, doesn't leak via logs.
//! - Roles are `user` and `model` (NOT `assistant`). System is a
//!   top-level `systemInstruction` field wrapped in a `Content` object.
//! - Content is `parts[]`, each part one of `{text}`, `{inlineData}`,
//!   `{functionCall}`, `{functionResponse}`, or `{text, thought: true}`
//!   (reasoning).
//! - Tool definitions are nested: `tools: [{ functionDeclarations: [...] }]`.
//! - Tool results are user-role messages with `functionResponse` parts.
//!   No separate `tool` role; consecutive Tool messages collapse into
//!   one user-role message with multiple parts (same idea as Anthropic
//!   but a different part type).
//! - Streaming uses untyped chunks: each SSE event is a complete partial
//!   `GenerateContentResponse` snapshot. We accumulate `parts[]` across
//!   chunks; no per-block-index typed-event state machine needed.
//! - Reasoning is gated by `generationConfig.thinkingConfig.thinkingBudget`
//!   (int) + `includeThoughts: bool`. Thoughts come back as parts with
//!   `thought: true`.
//! - **No signature round-trip.** Gemini has no encrypted server state
//!   for thinking blocks — multi-turn reasoning is stateless on the
//!   client side. This is significantly simpler than Anthropic.
//! - Tool call IDs are synthesized (`call_<n>`) since Gemini doesn't
//!   supply them; tool results match by **name** on the wire. The protocol has
//!   no call-id field on `functionCall`/`functionResponse`, so two parallel
//!   calls to the *same* tool in one turn are associated only by ORDER — we
//!   keep calls and results in arrival order so positional matching holds. This
//!   is an inherent Gemini limitation, not fixable in the wire format.
//!
//! # Caching note (Step 5b)
//!
//! Gemini 2.5+ enables **implicit caching** by default — repeated
//! content prefixes get cost discounts automatically with no client
//! code. The minimums (verified at ai.google.dev/gemini-api/docs/caching
//! as of 2026-04) are 1,024 tokens for Flash variants and 4,096 tokens
//! for Pro variants. Mermaid's static system prompt + tool definitions
//! (~5-7k tokens) clears both, so users get savings for free.
//!
//! **Explicit** caching via the `cachedContents` API is intentionally
//! NOT implemented here. The per-request lifecycle (create cache →
//! reuse `cachedContent` ID → invalidate on prompt change) adds
//! complexity for marginal gain over what implicit caching already
//! delivers. Revisit if Mermaid's prompt grows past ~32k tokens (where
//! implicit hit rates drop) or if users hit measurable cost issues.
//! When that day comes, the entry point is a `cachedContents` POST in
//! `send_chat`, returning a cache name to slot into `cachedContent`
//! field of subsequent `generateContent` requests.

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

/// Append `chunk` to `buf`, char-boundary-safe truncation at `cap` bytes.
/// Sets `*truncated` once tripped; subsequent calls become no-ops. Same
/// shape as the helpers in the other adapters.
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

/// Map Gemini's `finishReason` onto the normalized [`FinishReason`]. Gemini
/// reports tool calls with `STOP` (the call rides in `parts`), so there is no
/// `ToolUse` mapping; safety/recitation/blocklist reasons are content blocks.
/// Whether an empty (no-content) Gemini candidate/chunk is a benign normal
/// completion rather than a block to surface as an error. `FINISH_REASON_
/// UNSPECIFIED` is included — it's not a safety block, and intermediate stream
/// chunks legitimately carry it — so the streaming and non-streaming paths treat
/// an empty response identically (#51).
fn gemini_empty_is_benign(reason: &str) -> bool {
    matches!(reason, "STOP" | "MAX_TOKENS" | "FINISH_REASON_UNSPECIFIED")
}

fn map_gemini_finish_reason(s: &str) -> FinishReason {
    match s {
        "STOP" => FinishReason::Stop,
        "MAX_TOKENS" => FinishReason::Length,
        "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" => {
            FinishReason::ContentFilter
        },
        other => FinishReason::Other(other.to_string()),
    }
}

/// F56: whether a Gemini stream ended abnormally — it closed before the
/// terminal `finishReason` was ever observed on a chunk. Gemini ends every
/// normal stream (including tool-call turns, which report `STOP`) with a
/// candidate `finishReason`; there is no separate `[DONE]`-style frame, so the
/// `finishReason` IS the terminal marker. Its absence means the connection
/// dropped mid-response — surfacing a clean `Ok` (with `stop_reason: None`)
/// would be indistinguishable from a real completion, so the caller returns a
/// stream error. A `MAX_TOKENS` truncation sets a real `finishReason`
/// (`Length`), so it is NOT abnormal and is preserved.
fn stream_closed_abnormally(finish_reason: Option<&FinishReason>) -> bool {
    finish_reason.is_none()
}

/// Translate `ReasoningLevel` to Gemini 2.5's `thinkingBudget` (int
/// tokens). `-1` means adaptive (Gemini decides up to the model's
/// ceiling); `0` means disabled. Per-model floors are applied
/// separately by `gemini_thinking_dispatch` — this function returns the
/// raw mapping which gets clamped before going on the wire.
fn thinking_budget_for(level: ReasoningLevel) -> i32 {
    match level {
        ReasoningLevel::None => 0,
        ReasoningLevel::Minimal => 512,
        ReasoningLevel::Low => 2048,
        ReasoningLevel::Medium => 8192,
        ReasoningLevel::High => 24576,
        // Max and XHigh both map to the adaptive sentinel on Gemini 2.5.
        // Gemini has no xhigh tier so the two collapse to the same shape.
        ReasoningLevel::Max | ReasoningLevel::XHigh => -1,
    }
}

/// Translate `ReasoningLevel` to Gemini 3's `thinkingLevel` enum
/// string. Gemini 3 cannot truly disable thinking — `None` maps to
/// `"minimal"` (the documented closest-to-off; per Google's docs,
/// "the model likely will not think though it still potentially can").
/// Gemini 3 also has no `max` or `xhigh` tier; both collapse to `high`.
fn thinking_level_for(level: ReasoningLevel) -> &'static str {
    match level {
        ReasoningLevel::None | ReasoningLevel::Minimal => "minimal",
        ReasoningLevel::Low => "low",
        ReasoningLevel::Medium => "medium",
        ReasoningLevel::High | ReasoningLevel::Max | ReasoningLevel::XHigh => "high",
    }
}

/// Per-model thinking dispatch — the right-shaped `thinkingConfig`
/// payload depends on the model line. Gemini 3 swapped from the integer
/// `thinkingBudget` to a `thinkingLevel` enum (verified at
/// `ai.google.dev/gemini-api/docs/thinking`); older models don't
/// support thinking at all and would 400 if we sent one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GeminiThinkingDispatch {
    /// Model doesn't support thinking — omit `thinkingConfig` entirely
    /// (sending it causes a syntax error per the official docs).
    Disabled,
    /// Gemini 3.x: emit `thinkingLevel` (enum string).
    Level,
    /// Gemini 2.5: emit `thinkingBudget` (int) clamped to the model's
    /// supported range. `min` is the lowest non-zero budget the model
    /// accepts; `can_disable` is whether `thinkingBudget: 0` is valid.
    Budget { min: i32, can_disable: bool },
}

/// Pick the right thinking dispatch for the given Gemini model. Per-
/// model floors and disable-rules from `ai.google.dev/gemini-api/docs/
/// thinking`:
/// - Gemini 3.x → `thinkingLevel` enum (cannot truly disable).
/// - Gemini 2.5 Pro → `thinkingBudget`, range 128–32768, can't disable.
/// - Gemini 2.5 Flash → `thinkingBudget`, range 0–24576, can disable.
/// - Gemini 2.5 Flash Lite → `thinkingBudget`, range 512–24576 OR 0,
///   can disable (hard min 512 when on).
/// - Older models (2.0 and earlier) → no thinking; sending
///   `thinkingConfig` causes a syntax error per the docs.
fn gemini_thinking_dispatch(model: &str) -> GeminiThinkingDispatch {
    let m = model.to_lowercase();
    if m.starts_with("gemini-3") {
        return GeminiThinkingDispatch::Level;
    }
    if m.starts_with("gemini-2.5-pro") {
        return GeminiThinkingDispatch::Budget {
            min: 128,
            can_disable: false,
        };
    }
    // Flash-Lite must come BEFORE Flash since it shares the prefix.
    if m.starts_with("gemini-2.5-flash-lite") {
        return GeminiThinkingDispatch::Budget {
            min: 512,
            can_disable: true,
        };
    }
    if m.starts_with("gemini-2.5-flash") {
        return GeminiThinkingDispatch::Budget {
            min: 0,
            can_disable: true,
        };
    }
    GeminiThinkingDispatch::Disabled
}

/// Convert Mermaid's OpenAI-shaped tool definitions to Gemini's nested
/// `[{functionDeclarations: [{name, description, parameters}]}]` shape.
/// All declarations go into a single tool group.
fn to_gemini_tools(openai_tools: &[&Value]) -> Vec<Value> {
    let declarations: Vec<Value> = openai_tools
        .iter()
        .filter_map(|tool| {
            let function = tool.get("function")?;
            let name = function.get("name")?.as_str()?;
            let description = function
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("");
            let parameters = function.get("parameters").cloned().unwrap_or(json!({
                "type": "object",
                "properties": {}
            }));
            Some(json!({
                "name": name,
                "description": description,
                "parameters": parameters,
            }))
        })
        .collect();

    if declarations.is_empty() {
        Vec::new()
    } else {
        vec![json!({"functionDeclarations": declarations})]
    }
}

/// Translate Mermaid's `ChatMessage` history into Gemini's
/// `(systemInstruction, contents)` shape.
///
/// - `MessageRole::System` → top-level `systemInstruction` (first wins).
/// - `MessageRole::User` → `{role: "user", parts: [text + inlineData]}`.
/// - `MessageRole::Assistant` → `{role: "model", parts: [text + functionCall]}`.
///   Note the role rename: Mermaid's `Assistant` serializes as Gemini's
///   `model`. Thinking content (`msg.thinking`) re-emits as a `thought:
///   true` text part — stateless, no signature.
/// - `MessageRole::Tool` → user-role message with one `functionResponse`
///   part per tool result. Consecutive Tool messages merge into one
///   user-role message (same idea as Anthropic).
fn convert_messages(messages: &[ChatMessage]) -> (Option<Value>, Vec<Value>) {
    let mut system: Option<Value> = None;
    let mut out: Vec<Value> = Vec::new();

    let mut i = 0;
    while i < messages.len() {
        let msg = &messages[i];
        match msg.role {
            MessageRole::System => {
                if system.is_none() && !msg.content.is_empty() {
                    system = Some(json!({
                        "parts": [{"text": msg.content}],
                    }));
                }
                i += 1;
            },
            MessageRole::User => {
                let mut parts: Vec<Value> = Vec::new();
                if !msg.content.is_empty() {
                    parts.push(json!({"text": msg.content}));
                }
                if let Some(ref images) = msg.images {
                    for data in images {
                        parts.push(json!({
                            "inlineData": {
                                "mimeType": "image/png",
                                "data": data,
                            }
                        }));
                    }
                }
                if parts.is_empty() {
                    parts.push(json!({"text": ""}));
                }
                out.push(json!({"role": "user", "parts": parts}));
                i += 1;
            },
            MessageRole::Assistant => {
                let mut parts: Vec<Value> = Vec::new();
                if let Some(ref thinking) = msg.thinking
                    && !thinking.is_empty()
                {
                    // Re-emit prior reasoning as a thought part so
                    // the model sees the full chain on follow-up turns.
                    parts.push(json!({
                        "text": thinking,
                        "thought": true,
                    }));
                }
                if !msg.content.is_empty() {
                    parts.push(json!({"text": msg.content}));
                }
                if let Some(ref tool_calls) = msg.tool_calls {
                    for tc in tool_calls {
                        parts.push(json!({
                            "functionCall": {
                                "name": tc.function.name,
                                "args": tc.function.arguments,
                            }
                        }));
                    }
                }
                if parts.is_empty() {
                    // Skip empty assistant turns (shouldn't happen, but
                    // Gemini rejects role/parts with empty parts array).
                    i += 1;
                    continue;
                }
                out.push(json!({"role": "model", "parts": parts}));
                i += 1;
            },
            MessageRole::Tool => {
                // Merge consecutive Tool messages into one user-role
                // message containing multiple functionResponse parts.
                let mut parts: Vec<Value> = Vec::new();
                while i < messages.len() && messages[i].role == MessageRole::Tool {
                    let t = &messages[i];
                    let name = t
                        .tool_name
                        .clone()
                        .unwrap_or_else(|| "unknown_tool".to_string());
                    // Gemini expects `response` to be an object/value. We
                    // wrap the textual tool result in `{result: <text>}`
                    // so the model sees structured-but-typed content.
                    parts.push(json!({
                        "functionResponse": {
                            "name": name,
                            "response": {"result": t.content},
                        }
                    }));
                    i += 1;
                }
                out.push(json!({"role": "user", "parts": parts}));
            },
        }
    }

    (system, out)
}

/// Google Gemini adapter.
pub struct GeminiAdapter {
    client: Client,
    api_key: String,
    base_url: String,
    model_name: String,
    capabilities: ModelCapabilities,
}

impl GeminiAdapter {
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
                    backend: "gemini".to_string(),
                    url: base_url.clone(),
                    reason: e.to_string(),
                })
            })?;

        // Gemini 2.5+ and Gemini 3.x all accept `thinkingBudget` in
        // generationConfig. Models that don't actually do extended
        // thinking silently ignore it, so advertising the full enum
        // is forward-compatible. Gemini has no xhigh tier — `XHigh`
        // collapses to the model's top (Gemini 3: "high"; Gemini 2.5:
        // adaptive sentinel -1).
        let capabilities = ModelCapabilities {
            supports_tools: true,
            supports_vision: true,
            supports_reasoning: ReasoningCapability::Levels(vec![
                ReasoningLevel::None,
                ReasoningLevel::Minimal,
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

    /// Build the JSON request body for `:generateContent` /
    /// `:streamGenerateContent`. The model name lives in the URL, not
    /// the body, so it doesn't appear here.
    fn build_request_body(&self, messages: &[ChatMessage], config: &ModelConfig) -> Value {
        let (system_from_msgs, gemini_contents) = convert_messages(messages);
        // ModelConfig.system_prompt (+ optional MERMAID.md suffix) overrides
        // any system message in the history (matches Anthropic / OpenAI-compat
        // behavior). Gemini doesn't expose per-block cache markers in this
        // path, so the static base + dynamic suffix are concatenated with a
        // `---` separator via combined_system_prompt().
        let system = match (config.combined_system_prompt(), system_from_msgs) {
            (Some(s), _) if !s.is_empty() => Some(json!({
                "parts": [{"text": s}],
            })),
            (_, Some(v)) => Some(v),
            _ => None,
        };

        let mut body = json!({
            "contents": gemini_contents,
        });
        if let Some(s) = system {
            body["systemInstruction"] = s;
        }

        // generationConfig: temperature, max_tokens, thinkingConfig.
        let mut gen_config = json!({});
        // Gemini accepts 0.0..=2.0 — same as OpenAI; no clamping needed
        // beyond what the user already validated, but be defensive.
        gen_config["temperature"] = json!(config.temperature.clamp(0.0, 2.0));
        if config.max_tokens > 0 {
            gen_config["maxOutputTokens"] = json!(config.max_tokens);
        }

        // Reasoning: snap onto supported levels first (defensive — the
        // adapter advertises the full enum, but a future per-model
        // capability shrink lands cleanly through this path).
        let effective_reasoning = match &self.capabilities.supports_reasoning {
            ReasoningCapability::Levels(supported) => {
                nearest_effort(config.reasoning, supported).unwrap_or(ReasoningLevel::None)
            },
            _ => config.reasoning,
        };

        // Per-model thinking dispatch (Step 5c bug fix). Gemini 3 uses
        // `thinkingLevel` enum; 2.5 uses `thinkingBudget` int with per-
        // model floors + can-disable rules; older models don't support
        // thinkingConfig at all and would 400 if we sent one.
        match gemini_thinking_dispatch(&self.model_name) {
            GeminiThinkingDispatch::Disabled => {
                // Don't set thinkingConfig — this model would 400.
            },
            GeminiThinkingDispatch::Level => {
                let level_str = thinking_level_for(effective_reasoning);
                gen_config["thinkingConfig"] = json!({
                    "thinkingLevel": level_str,
                    "includeThoughts": effective_reasoning != ReasoningLevel::None,
                });
            },
            GeminiThinkingDispatch::Budget { min, can_disable } => {
                let raw = thinking_budget_for(effective_reasoning);
                let budget = if effective_reasoning == ReasoningLevel::None {
                    // None: disable entirely if the model allows;
                    // otherwise force the minimum (e.g. 2.5 Pro can't
                    // disable, must send at least 128).
                    if can_disable { 0 } else { min }
                } else if raw < 0 {
                    // -1 (adaptive sentinel for Max) — pass through.
                    -1
                } else {
                    // Clamp UP to the model's minimum if we're below it.
                    raw.max(min)
                };
                gen_config["thinkingConfig"] = json!({
                    "thinkingBudget": budget,
                    "includeThoughts": budget != 0,
                });
            },
        }
        body["generationConfig"] = gen_config;

        // Tools come from `config.tools` (OpenAI-compat shape,
        // populated by the provider wrapper). Drop web tools when no
        // cloud key is set.
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
        let gemini_tools = to_gemini_tools(&filtered);
        if !gemini_tools.is_empty() {
            body["tools"] = json!(gemini_tools);
        }

        body
    }

    /// POST to `:generateContent` (sync) or `:streamGenerateContent`
    /// (streaming) and return the raw response.
    /// Transparently retries on 5xx, 429, or reqwest connect failures
    /// via `crate::effect::retry_transient_http`.
    async fn send_chat(&self, body: &Value, stream: bool) -> Result<reqwest::Response> {
        let method = if stream {
            "streamGenerateContent?alt=sse"
        } else {
            "generateContent"
        };
        let url = format!(
            "{}/models/{}:{}",
            self.base_url.trim_end_matches('/'),
            self.model_name,
            method
        );
        crate::effect::retry_transient_http(|| async {
            self.client
                .post(&url)
                .header("x-goog-api-key", &self.api_key)
                .header("content-type", "application/json")
                .json(body)
                .send()
                .await
                .map_err(|e| {
                    ModelError::Backend(BackendError::ConnectionFailed {
                        backend: "gemini".to_string(),
                        url: url.clone(),
                        reason: e.to_string(),
                    })
                })
        })
        .await
    }

    /// Decode a non-streaming response into `ModelResponse`.
    async fn decode_non_streaming(&self, response: reqwest::Response) -> Result<ModelResponse> {
        if !response.status().is_success() {
            return Err(http_error_from_response(response).await);
        }

        let json: GeminiResponse = response.json().await.map_err(|e| ModelError::ParseError {
            message: format!("Failed to parse Gemini response: {}", e),
            raw: None,
        })?;

        // F52: a prompt-level safety block returns `promptFeedback.blockReason`
        // with NO candidates. Without this guard the `candidates.into_iter()
        // .next()` below is `None`, the block is skipped, and we return an empty
        // `Ok` with `stop_reason: None`. Surface a typed refusal instead — the
        // candidate-level block (no `content` on a present candidate) is handled
        // separately further down.
        if json.candidates.is_empty()
            && let Some(reason) = json
                .prompt_feedback
                .as_ref()
                .and_then(|pf| pf.block_reason.as_deref())
        {
            return Err(ModelError::Backend(BackendError::ProviderError {
                provider: "gemini".to_string(),
                code: Some(reason.to_string()),
                message: format!("Gemini blocked the prompt (blockReason={reason})"),
            }));
        }

        let mut text_acc = String::new();
        let mut thinking_acc = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut stop_reason: Option<FinishReason> = None;

        if let Some(candidate) = json.candidates.into_iter().next() {
            stop_reason = candidate
                .finish_reason
                .as_deref()
                .map(map_gemini_finish_reason);
            match candidate.content {
                Some(content) => {
                    for part in content.parts {
                        if let Some(text) = part.text {
                            if part.thought.unwrap_or(false) {
                                thinking_acc.push_str(&text);
                            } else {
                                text_acc.push_str(&text);
                            }
                        } else if let Some(fc) = part.function_call {
                            let id = format!("call_{}", tool_calls.len());
                            tool_calls.push(ToolCall {
                                id: Some(id),
                                function: FunctionCall {
                                    name: fc.name,
                                    arguments: fc.args,
                                },
                            });
                        }
                    }
                },
                None => {
                    // No content: this is a block (safety/recitation/etc.), not
                    // a parse failure. Surface a clear error keyed off the
                    // finishReason rather than returning an empty success.
                    let reason = candidate.finish_reason.as_deref().unwrap_or("unknown");
                    if !gemini_empty_is_benign(reason) {
                        return Err(ModelError::Backend(BackendError::ProviderError {
                            provider: "gemini".to_string(),
                            code: Some(reason.to_string()),
                            message: format!("Gemini returned no content (finishReason={reason})"),
                        }));
                    }
                },
            }
        }

        let raw_prompt_tokens = json.usage_metadata.prompt_token_count.unwrap_or(0);
        let cached_tokens = json.usage_metadata.cached_content_token_count.unwrap_or(0);
        // Gemini's promptTokenCount INCLUDES cachedContentTokenCount; subtract it
        // so the input breakdown (fresh prompt + cached) isn't double-counted
        // (#137), matching openai_compat's token_usage_from_wire.
        let prompt_tokens = raw_prompt_tokens.saturating_sub(cached_tokens);
        let completion_tokens = json.usage_metadata.candidates_token_count.unwrap_or(0);
        let reasoning_tokens = json.usage_metadata.thoughts_token_count.unwrap_or(0);
        let usage = TokenUsage::provider(
            prompt_tokens,
            completion_tokens,
            json.usage_metadata.total_token_count.unwrap_or_else(|| {
                // The total uses the FULL prompt (incl. cached), not the de-duped
                // breakdown component.
                raw_prompt_tokens
                    .saturating_add(completion_tokens)
                    .saturating_add(reasoning_tokens)
            }),
        )
        .with_cached_input(cached_tokens)
        .with_reasoning_output(reasoning_tokens);

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
            // Gemini has no signature round-trip — leave None.
            thinking_signature: None,
        })
    }

    /// Stream the response, emit typed events, return the final
    /// `ModelResponse`.
    ///
    /// Gemini's streaming model is much simpler than Anthropic's: each
    /// SSE event is a complete partial `GenerateContentResponse` snapshot
    /// (one candidate's parts so far, plus optional usageMetadata). We
    /// walk `candidates[0].content.parts[]` and dispatch per-part —
    /// no per-block-index state machine needed.
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
        let mut state = StreamState::default();

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
                process_chunk_payload(&payload, &mut state, &callback, hide_reasoning_trace)?;
            }
        }

        // F56: a stream that ended before any candidate `finishReason` was
        // dropped mid-response. Surface a stream error instead of a clean `Ok`
        // (with `stop_reason: None`) that would be indistinguishable from a real
        // completion. A `MAX_TOKENS` truncation set a real `finishReason`, so it
        // does NOT trip this and is preserved.
        if stream_closed_abnormally(state.finish_reason.as_ref()) {
            return Err(ModelError::StreamError(
                "Gemini stream closed before a terminal finishReason; the \
                 connection was likely dropped mid-response"
                    .to_string(),
            ));
        }

        let total_tokens = if state.total_tokens > 0 {
            state.total_tokens
        } else {
            state
                .prompt_tokens
                .saturating_add(state.completion_tokens)
                .saturating_add(state.reasoning_output_tokens)
        };
        // F3: wrapper emits the authoritative `Done`. See
        // adapters/anthropic.rs for rationale.

        let usage = state.usage(total_tokens);

        Ok(ModelResponse {
            content: state.text_acc,
            usage,
            model_name: self.model_name.clone(),
            stop_reason: state.finish_reason,
            thinking: if state.thinking_acc.is_empty() {
                None
            } else {
                Some(state.thinking_acc)
            },
            tool_calls: if state.tool_calls_done.is_empty() {
                None
            } else {
                Some(state.tool_calls_done)
            },
            thinking_signature: None,
        })
    }
}

/// Mutable accumulator state threaded through `process_chunk_payload`.
/// Extracted from `handle_stream` so the per-payload dispatch can be
/// tested directly with synthetic SSE event sequences.
#[derive(Debug, Default)]
struct StreamState {
    text_acc: String,
    thinking_acc: String,
    tool_calls_done: Vec<ToolCall>,
    truncated: bool,
    prompt_tokens: usize,
    completion_tokens: usize,
    cached_input_tokens: usize,
    reasoning_output_tokens: usize,
    total_tokens: usize,
    /// Set once a `usageMetadata` block is seen, so a stream that never reports
    /// usage returns `None` instead of a misleading zero (#125).
    saw_usage: bool,
    finish_reason: Option<FinishReason>,
}

impl StreamState {
    /// Build the response usage. `None` when no `usageMetadata` arrived (#125),
    /// so the reducer keeps its estimate rather than resetting to zero. The
    /// fresh-prompt component subtracts cached, which Gemini folds into
    /// `promptTokenCount`, so the input breakdown isn't double-counted (#137).
    fn usage(&self, total_tokens: usize) -> Option<TokenUsage> {
        self.saw_usage.then(|| {
            let fresh_prompt = self.prompt_tokens.saturating_sub(self.cached_input_tokens);
            TokenUsage::provider(fresh_prompt, self.completion_tokens, total_tokens)
                .with_cached_input(self.cached_input_tokens)
                .with_reasoning_output(self.reasoning_output_tokens)
        })
    }
}

/// Process one SSE event payload (already JSON-decoded). Mutates `state`
/// and emits StreamEvents through `callback`. Returns Err on mid-stream
/// error payloads or JSON parse failure.
fn process_chunk_payload(
    payload: &str,
    state: &mut StreamState,
    callback: &StreamCallback,
    hide_reasoning_trace: bool,
) -> Result<()> {
    let parsed: Value = serde_json::from_str(payload).map_err(|e| ModelError::ParseError {
        message: format!("Failed to parse Gemini stream chunk: {}", e),
        raw: Some(payload.to_string()),
    })?;

    // Mid-stream error payload (rate limit, quota, etc.).
    if let Some(err) = parsed.get("error") {
        let code = err
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("UNKNOWN");
        let msg = err
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("Gemini stream error");
        return Err(ModelError::Backend(BackendError::ProviderError {
            provider: "gemini".to_string(),
            code: Some(code.to_string()),
            message: msg.to_string(),
        }));
    }

    // F52: a prompt-level safety block streams as a chunk carrying
    // `promptFeedback.blockReason` and NO candidates. The no-parts branch below
    // would otherwise swallow it as a benign empty success — there's no
    // candidate `finishReason` to trip its block check. Surface a typed refusal,
    // matching `decode_non_streaming`.
    if let Some(reason) = parsed
        .pointer("/promptFeedback/blockReason")
        .and_then(|v| v.as_str())
    {
        return Err(ModelError::Backend(BackendError::ProviderError {
            provider: "gemini".to_string(),
            code: Some(reason.to_string()),
            message: format!("Gemini blocked the prompt (blockReason={reason})"),
        }));
    }

    // Usage: any chunk may carry it; the last chunk is final.
    if let Some(usage) = parsed.get("usageMetadata") {
        state.saw_usage = true;
        if let Some(p) = usage.get("promptTokenCount").and_then(|v| v.as_u64()) {
            state.prompt_tokens = p as usize;
        }
        if let Some(c) = usage.get("candidatesTokenCount").and_then(|v| v.as_u64()) {
            state.completion_tokens = c as usize;
        }
        if let Some(t) = usage.get("totalTokenCount").and_then(|v| v.as_u64()) {
            state.total_tokens = t as usize;
        }
        if let Some(cached) = usage
            .get("cachedContentTokenCount")
            .and_then(|v| v.as_u64())
        {
            state.cached_input_tokens = cached as usize;
        }
        if let Some(thoughts) = usage.get("thoughtsTokenCount").and_then(|v| v.as_u64()) {
            state.reasoning_output_tokens = thoughts as usize;
        }
    }

    // Terminal finishReason rides on the candidate (may co-arrive with the
    // final parts, or alone on a content-free block). Record it either way.
    let finish_reason = parsed
        .pointer("/candidates/0/finishReason")
        .and_then(|v| v.as_str());
    if let Some(fr) = finish_reason {
        state.finish_reason = Some(map_gemini_finish_reason(fr));
    }

    // Walk parts. Each chunk's parts are NEW content (concatenated
    // client-side) — Gemini does not echo prior parts in subsequent
    // chunks.
    let Some(parts_arr) = parsed
        .pointer("/candidates/0/content/parts")
        .and_then(|v| v.as_array())
    else {
        // No parts. A terminal block (safety/recitation/etc.) that produced no
        // content must surface as an error — matching the non-streaming path —
        // rather than a silent empty success. A normal STOP/MAX_TOKENS chunk
        // with no parts (final usage-only chunk) is fine.
        if let Some(fr) = finish_reason
            && !gemini_empty_is_benign(fr)
            && state.text_acc.is_empty()
            && state.tool_calls_done.is_empty()
        {
            return Err(ModelError::Backend(BackendError::ProviderError {
                provider: "gemini".to_string(),
                code: Some(fr.to_string()),
                message: format!("Gemini returned no content (finishReason={fr})"),
            }));
        }
        return Ok(());
    };

    for part in parts_arr {
        // Function call part — emit immediately. Args arrive as a full
        // Value object, not fragmented JSON strings (unlike OpenAI).
        if let Some(fc) = part.get("functionCall") {
            let name = fc
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let args = fc.get("args").cloned().unwrap_or_else(|| json!({}));
            if name.is_empty() {
                continue;
            }
            let id = format!("call_{}", state.tool_calls_done.len());
            let tc = ToolCall {
                id: Some(id),
                function: FunctionCall {
                    name,
                    arguments: args,
                },
            };
            callback(StreamEvent::ToolCall(tc.clone()));
            state.tool_calls_done.push(tc);
            continue;
        }

        // Text part — possibly with thought: true flag.
        let Some(text) = part.get("text").and_then(|v| v.as_str()) else {
            continue;
        };
        if text.is_empty() || state.truncated {
            continue;
        }
        let is_thought = part
            .get("thought")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if is_thought {
            if !hide_reasoning_trace {
                callback(StreamEvent::Reasoning(ReasoningChunk {
                    text: text.to_string(),
                    signature: None,
                }));
            }
            push_capped(
                &mut state.thinking_acc,
                text,
                &mut state.truncated,
                MAX_RESPONSE_CHARS,
            );
        } else {
            callback(StreamEvent::Text(text.to_string()));
            push_capped(
                &mut state.text_acc,
                text,
                &mut state.truncated,
                MAX_RESPONSE_CHARS,
            );
        }
    }
    Ok(())
}

#[async_trait]
impl Model for GeminiAdapter {
    fn name(&self) -> &str {
        &self.model_name
    }

    fn capabilities(&self) -> &ModelCapabilities {
        &self.capabilities
    }

    /// Gemini does expose `/v1beta/models` for discovery, but Mermaid's
    /// per-provider model list is curated in ``providers::factory::ProviderFactory``
    /// to keep `mermaid list` snappy and predictable. Surface the
    /// adapter-level fact rather than drift from that.
    async fn list_models(&self) -> Result<Vec<String>> {
        Err(ModelError::Unsupported {
            feature: "list_models (gemini)".to_string(),
        })
    }

    async fn chat(
        &self,
        messages: &[ChatMessage],
        config: &ModelConfig,
        callback: Option<StreamCallback>,
    ) -> Result<ModelResponse> {
        let body = self.build_request_body(messages, config);
        let stream = callback.is_some();
        let response = self.send_chat(&body, stream).await?;
        if let Some(cb) = callback {
            self.handle_stream(response, cb, config.hide_reasoning_trace)
                .await
        } else {
            self.decode_non_streaming(response).await
        }
    }
}

// ===== Wire types =====

#[derive(Debug, Deserialize)]
struct GeminiResponse {
    #[serde(default)]
    candidates: Vec<Candidate>,
    #[serde(default, rename = "usageMetadata")]
    usage_metadata: UsageMetadata,
    // A prompt-level safety block returns `promptFeedback.blockReason` and NO
    // candidates (F52). Captured so `decode_non_streaming` can surface a typed
    // refusal instead of an empty `Ok`.
    #[serde(default, rename = "promptFeedback")]
    prompt_feedback: Option<PromptFeedback>,
}

/// Prompt-level safety feedback. When Gemini blocks the *prompt* itself (rather
/// than filtering a candidate's output), the response carries this with a
/// `blockReason` and an empty `candidates` array.
#[derive(Debug, Deserialize)]
struct PromptFeedback {
    #[serde(default, rename = "blockReason")]
    block_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Candidate {
    // Optional: a safety/recitation/other block returns a candidate with a
    // `finishReason` and NO `content`. Making this required turned such a
    // well-formed (blocked) response into a misleading whole-response parse
    // failure.
    #[serde(default)]
    content: Option<CandidateContent>,
    #[serde(default, rename = "finishReason")]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CandidateContent {
    #[serde(default)]
    parts: Vec<ResponsePart>,
}

/// Output part. Gemini parts have one of: `text` (with optional
/// `thought: true`), `functionCall`, `inlineData`, `executableCode`,
/// `codeExecutionResult`, etc. We model the two we consume; everything
/// else is silently ignored via serde defaults.
#[derive(Debug, Deserialize)]
struct ResponsePart {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    thought: Option<bool>,
    #[serde(default, rename = "functionCall")]
    function_call: Option<FunctionCallOut>,
}

#[derive(Debug, Deserialize)]
struct FunctionCallOut {
    name: String,
    #[serde(default)]
    args: Value,
}

#[derive(Debug, Default, Deserialize)]
struct UsageMetadata {
    #[serde(default, rename = "promptTokenCount")]
    prompt_token_count: Option<usize>,
    #[serde(default, rename = "candidatesTokenCount")]
    candidates_token_count: Option<usize>,
    #[serde(default, rename = "cachedContentTokenCount")]
    cached_content_token_count: Option<usize>,
    #[serde(default, rename = "thoughtsTokenCount")]
    thoughts_token_count: Option<usize>,
    #[serde(default, rename = "totalTokenCount")]
    total_token_count: Option<usize>,
}

/// Translate a non-success HTTP response into a structured `ModelError`.
async fn http_error_from_response(response: reqwest::Response) -> ModelError {
    let status = response.status().as_u16();
    let body = response
        .text()
        .await
        .unwrap_or_else(|_| "Unknown error".to_string());
    if let Ok(parsed) = serde_json::from_str::<Value>(&body)
        && let Some(err) = parsed.get("error")
    {
        let code = err.get("status").and_then(|v| v.as_str()).map(String::from);
        let msg = err
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or(&body)
            .to_string();
        // PERMISSION_DENIED on Gemini almost always means the API key
        // is invalid or the project doesn't have the API enabled.
        let suffix = if code.as_deref() == Some("PERMISSION_DENIED") {
            " (check that GOOGLE_API_KEY is valid and the Generative Language API is enabled)"
        } else if code.as_deref() == Some("INVALID_ARGUMENT")
            && msg.to_lowercase().contains("thinkingbudget")
        {
            " (thinkingBudget out of range for this model — file an issue at github.com/noahsabaj/mermaid)"
        } else {
            ""
        };
        return ModelError::Backend(BackendError::ProviderError {
            provider: "gemini".to_string(),
            code,
            message: format!("{}{}", msg, suffix),
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
    use crate::models::tool_call::{FunctionCall, ToolCall};

    #[test]
    fn candidate_without_content_deserializes() {
        // H23: a safety/recitation block returns a candidate with a
        // finishReason and NO content — it must parse, not fail.
        let json = serde_json::json!({
            "candidates": [{ "finishReason": "SAFETY" }],
            "usageMetadata": {}
        });
        let resp: GeminiResponse = serde_json::from_value(json).expect("blocked response parses");
        assert_eq!(resp.candidates.len(), 1);
        assert!(resp.candidates[0].content.is_none());
        assert_eq!(resp.candidates[0].finish_reason.as_deref(), Some("SAFETY"));
    }

    #[test]
    fn empty_response_benign_only_for_non_blocks() {
        // #51: both the streaming and non-streaming paths now treat an empty
        // response as benign for these reasons (UNSPECIFIED included — not a
        // block); a real block like SAFETY/RECITATION still surfaces an error.
        assert!(gemini_empty_is_benign("STOP"));
        assert!(gemini_empty_is_benign("MAX_TOKENS"));
        assert!(gemini_empty_is_benign("FINISH_REASON_UNSPECIFIED"));
        assert!(!gemini_empty_is_benign("SAFETY"));
        assert!(!gemini_empty_is_benign("RECITATION"));
    }

    #[test]
    fn prompt_block_response_parses_with_no_candidates() {
        // F52: a prompt-level block returns `promptFeedback.blockReason` and NO
        // candidates. The wire type must capture it so `decode_non_streaming`
        // can surface a refusal instead of an empty Ok.
        let json = serde_json::json!({
            "promptFeedback": { "blockReason": "SAFETY" }
        });
        let resp: GeminiResponse = serde_json::from_value(json).expect("prompt block parses");
        assert!(resp.candidates.is_empty());
        assert_eq!(
            resp.prompt_feedback
                .as_ref()
                .and_then(|pf| pf.block_reason.as_deref()),
            Some("SAFETY")
        );
    }

    #[test]
    fn stream_prompt_block_surfaces_provider_error() {
        // F52 (streaming): a chunk carrying `promptFeedback.blockReason` with no
        // candidates must surface a typed ProviderError, not be swallowed by the
        // no-parts branch as a silent empty success.
        let mut state = StreamState::default();
        let cb: StreamCallback = std::sync::Arc::new(|_ev| {});
        let payload = r#"{"promptFeedback":{"blockReason":"PROHIBITED_CONTENT"}}"#;
        let err = process_chunk_payload(payload, &mut state, &cb, false)
            .expect_err("prompt block must error");
        match err {
            ModelError::Backend(BackendError::ProviderError {
                provider,
                code,
                message,
            }) => {
                assert_eq!(provider, "gemini");
                assert_eq!(code.as_deref(), Some("PROHIBITED_CONTENT"));
                assert!(message.contains("PROHIBITED_CONTENT"), "{message}");
            },
            other => panic!("expected ProviderError, got {other:?}"),
        }
    }

    #[test]
    fn stream_prompt_feedback_without_block_reason_is_not_an_error() {
        // A `promptFeedback` block that only echoes safety ratings (no
        // `blockReason`) must NOT be treated as a block — it co-occurs with real
        // content on normal responses.
        let mut state = StreamState::default();
        let cb: StreamCallback = std::sync::Arc::new(|_ev| {});
        let payload = r#"{"promptFeedback":{"safetyRatings":[]},"candidates":[{"content":{"parts":[{"text":"hello"}]}}]}"#;
        process_chunk_payload(payload, &mut state, &cb, false).expect("benign feedback is ok");
        assert_eq!(state.text_acc, "hello");
    }

    fn test_adapter() -> GeminiAdapter {
        GeminiAdapter::new(
            "test-key".to_string(),
            "gemini-3-pro".to_string(),
            "https://generativelanguage.googleapis.com/v1beta".to_string(),
        )
        .expect("adapter constructs")
    }

    // --- thinking_budget_for ---

    #[test]
    fn thinking_budget_per_level() {
        assert_eq!(thinking_budget_for(ReasoningLevel::None), 0);
        assert_eq!(thinking_budget_for(ReasoningLevel::Minimal), 512);
        assert_eq!(thinking_budget_for(ReasoningLevel::Low), 2048);
        assert_eq!(thinking_budget_for(ReasoningLevel::Medium), 8192);
        assert_eq!(thinking_budget_for(ReasoningLevel::High), 24576);
        assert_eq!(thinking_budget_for(ReasoningLevel::Max), -1);
    }

    // --- to_gemini_tools ---

    #[test]
    fn tool_translation_groups_into_function_declarations() {
        let openai = [
            json!({
                "type": "function",
                "function": {
                    "name": "read_file",
                    "description": "Read a file",
                    "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}
                }
            }),
            json!({
                "type": "function",
                "function": {
                    "name": "write_file",
                    "description": "Write a file",
                    "parameters": {"type": "object", "properties": {}}
                }
            }),
        ];
        let refs: Vec<&Value> = openai.iter().collect();
        let gemini = to_gemini_tools(&refs);
        assert_eq!(gemini.len(), 1);
        let decls = gemini[0]["functionDeclarations"].as_array().unwrap();
        assert_eq!(decls.len(), 2);
        assert_eq!(decls[0]["name"], "read_file");
        assert_eq!(decls[0]["description"], "Read a file");
        assert!(decls[0].get("parameters").is_some());
        // No OpenAI wrapper should leak through.
        assert!(decls[0].get("function").is_none());
        assert!(decls[0].get("type").is_none());
    }

    #[test]
    fn tool_translation_handles_missing_description() {
        let openai = [json!({
            "type": "function",
            "function": {
                "name": "no_desc",
                "parameters": {"type": "object", "properties": {}}
            }
        })];
        let refs: Vec<&Value> = openai.iter().collect();
        let gemini = to_gemini_tools(&refs);
        let decls = gemini[0]["functionDeclarations"].as_array().unwrap();
        assert_eq!(decls[0]["description"], "");
    }

    #[test]
    fn tool_translation_empty_returns_empty() {
        let gemini = to_gemini_tools(&[]);
        assert!(gemini.is_empty());
    }

    // --- convert_messages ---

    #[test]
    fn convert_messages_extracts_system_first() {
        let messages = vec![
            ChatMessage::system("You are helpful."),
            ChatMessage::user("hi"),
            ChatMessage::system("ignored second system"),
        ];
        let (system, contents) = convert_messages(&messages);
        let sys = system.expect("system extracted");
        assert_eq!(sys["parts"][0]["text"], "You are helpful.");
        // System messages are NOT included in contents.
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0]["role"], "user");
    }

    #[test]
    fn convert_messages_renames_assistant_to_model() {
        let messages = vec![ChatMessage::user("hi"), ChatMessage::assistant("hello")];
        let (_system, contents) = convert_messages(&messages);
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[1]["role"], "model");
        assert_eq!(contents[1]["parts"][0]["text"], "hello");
    }

    #[test]
    fn convert_messages_merges_consecutive_tool_messages() {
        let messages = vec![
            ChatMessage::user("read two files"),
            ChatMessage::tool("call_0", "read_file", "contents of A"),
            ChatMessage::tool("call_1", "read_file", "contents of B"),
            ChatMessage::user("now compare them"),
        ];
        let (_, contents) = convert_messages(&messages);
        // user → user (merged tool results) → user
        assert_eq!(contents.len(), 3);
        assert_eq!(contents[1]["role"], "user");
        let parts = contents[1]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 2, "two functionResponse parts");
        assert_eq!(parts[0]["functionResponse"]["name"], "read_file");
        assert_eq!(
            parts[0]["functionResponse"]["response"]["result"],
            "contents of A"
        );
        assert_eq!(
            parts[1]["functionResponse"]["response"]["result"],
            "contents of B"
        );
    }

    #[test]
    fn convert_messages_emits_function_call_part_for_assistant_tool_call() {
        let mut msg = ChatMessage::assistant("");
        msg.tool_calls = Some(vec![ToolCall {
            id: Some("call_0".to_string()),
            function: FunctionCall {
                name: "read_file".to_string(),
                arguments: json!({"path": "Cargo.toml"}),
            },
        }]);
        let messages = vec![ChatMessage::user("read it"), msg];
        let (_, contents) = convert_messages(&messages);
        assert_eq!(contents[1]["role"], "model");
        let parts = contents[1]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 1);
        let fc = &parts[0]["functionCall"];
        assert_eq!(fc["name"], "read_file");
        assert_eq!(fc["args"]["path"], "Cargo.toml");
    }

    #[test]
    fn convert_messages_emits_inline_data_for_user_images() {
        let msg = ChatMessage::user("look at this").with_images(vec!["base64data".to_string()]);
        let (_, contents) = convert_messages(&[msg]);
        let parts = contents[0]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["text"], "look at this");
        assert_eq!(parts[1]["inlineData"]["mimeType"], "image/png");
        assert_eq!(parts[1]["inlineData"]["data"], "base64data");
    }

    #[test]
    fn convert_messages_emits_thought_part_for_assistant_thinking() {
        let mut msg = ChatMessage::assistant("the answer is 42");
        msg.thinking = Some("step 1: think hard".to_string());
        let messages = vec![ChatMessage::user("compute"), msg];
        let (_, contents) = convert_messages(&messages);
        let parts = contents[1]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["text"], "step 1: think hard");
        assert_eq!(parts[0]["thought"], true);
        assert_eq!(parts[1]["text"], "the answer is 42");
        // No `thought` flag on the answer part.
        assert!(parts[1].get("thought").is_none());
    }

    // --- capabilities & name ---

    #[test]
    fn capabilities_advertise_full_reasoning_levels_and_vision() {
        let adapter = test_adapter();
        let caps = adapter.capabilities();
        assert!(caps.supports_tools);
        assert!(caps.supports_vision);
        match &caps.supports_reasoning {
            ReasoningCapability::Levels(levels) => {
                assert!(levels.contains(&ReasoningLevel::None));
                assert!(levels.contains(&ReasoningLevel::Minimal));
                assert!(levels.contains(&ReasoningLevel::Max));
            },
            other => panic!("expected Levels, got {:?}", other),
        }
    }

    #[test]
    fn name_returns_model_id() {
        let adapter = test_adapter();
        assert_eq!(adapter.name(), "gemini-3-pro");
    }

    // --- build_request_body ---

    #[test]
    fn build_request_body_includes_required_fields() {
        let adapter = test_adapter();
        let messages = vec![ChatMessage::user("hi")];
        let config = ModelConfig::default();
        let body = adapter.build_request_body(&messages, &config);
        // Model name lives in the URL, not the body — verify it's not here.
        assert!(body.get("model").is_none());
        assert!(body["contents"].is_array());
        let contents = body["contents"].as_array().unwrap();
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[0]["parts"][0]["text"], "hi");
        assert!(body["generationConfig"].is_object());
    }

    #[test]
    fn build_request_body_wraps_system_in_content_object() {
        let adapter = test_adapter();
        let messages = vec![ChatMessage::user("hi")];
        let config = ModelConfig {
            system_prompt: Some("You are helpful.".to_string()),
            ..Default::default()
        };
        let body = adapter.build_request_body(&messages, &config);
        let sys = &body["systemInstruction"];
        assert!(sys.is_object());
        assert_eq!(sys["parts"][0]["text"], "You are helpful.");
    }

    /// Step 5h: Gemini doesn't expose per-block cache markers in this path.
    /// The dynamic MERMAID.md suffix is concatenated onto the static system
    /// instruction with a `---` separator. Both halves reach the model in
    /// one systemInstruction payload.
    #[test]
    fn build_request_body_concats_dynamic_suffix_to_system_instruction() {
        let adapter = test_adapter();
        let messages = vec![ChatMessage::user("hi")];
        let config = ModelConfig {
            system_prompt: Some("You are Mermaid.".to_string()),
            dynamic_system_suffix: Some("Project rule: always snake_case.".to_string()),
            ..Default::default()
        };
        let body = adapter.build_request_body(&messages, &config);
        let text = body["systemInstruction"]["parts"][0]["text"]
            .as_str()
            .expect("systemInstruction text");
        assert!(text.contains("You are Mermaid."));
        assert!(text.contains("Project rule: always snake_case."));
        assert!(text.contains("---"));
    }

    /// Step 5c: gemini-3-pro (the test adapter's model) now uses
    /// `thinkingLevel` enum, not `thinkingBudget` int. Same Medium
    /// reasoning request maps to `thinkingLevel: "medium"`.
    #[test]
    fn build_request_body_thinking_level_for_medium_on_gemini_3() {
        let adapter = test_adapter(); // gemini-3-pro
        let messages = vec![ChatMessage::user("hi")];
        let config = ModelConfig {
            reasoning: ReasoningLevel::Medium,
            ..Default::default()
        };
        let body = adapter.build_request_body(&messages, &config);
        let tc = &body["generationConfig"]["thinkingConfig"];
        assert_eq!(tc["thinkingLevel"], "medium");
        assert_eq!(tc["includeThoughts"], true);
        // No thinkingBudget on Gemini 3 — it's the wrong field.
        assert!(tc.get("thinkingBudget").is_none());
    }

    /// Step 5c: Gemini 3 has no `max` tier — Max collapses to `high`.
    #[test]
    fn build_request_body_thinking_level_for_max_collapses_to_high_on_gemini_3() {
        let adapter = test_adapter(); // gemini-3-pro
        let messages = vec![ChatMessage::user("hi")];
        let config = ModelConfig {
            reasoning: ReasoningLevel::Max,
            ..Default::default()
        };
        let body = adapter.build_request_body(&messages, &config);
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "high"
        );
    }

    /// Step 5c: Gemini 3 cannot truly disable thinking — `None` maps to
    /// `thinkingLevel: "minimal"` (closest-to-off per Google's docs).
    #[test]
    fn build_request_body_thinking_level_minimal_for_none_on_gemini_3() {
        let adapter = test_adapter(); // gemini-3-pro
        let messages = vec![ChatMessage::user("hi")];
        let config = ModelConfig {
            reasoning: ReasoningLevel::None,
            ..Default::default()
        };
        let body = adapter.build_request_body(&messages, &config);
        let tc = &body["generationConfig"]["thinkingConfig"];
        assert_eq!(tc["thinkingLevel"], "minimal");
        // includeThoughts is false when level == None.
        assert_eq!(tc["includeThoughts"], false);
    }

    /// Step 5c: gemini-2.5-pro uses thinkingBudget int with floor 128.
    /// `--reasoning none` clamps UP to 128 (can't actually disable).
    #[test]
    fn build_request_body_thinking_budget_clamps_to_min_128_on_gemini_2_5_pro_for_none() {
        let adapter = GeminiAdapter::new(
            "test-key".to_string(),
            "gemini-2.5-pro".to_string(),
            "https://generativelanguage.googleapis.com/v1beta".to_string(),
        )
        .expect("adapter constructs");
        let messages = vec![ChatMessage::user("hi")];
        let config = ModelConfig {
            reasoning: ReasoningLevel::None,
            ..Default::default()
        };
        let body = adapter.build_request_body(&messages, &config);
        let tc = &body["generationConfig"]["thinkingConfig"];
        // Pro can't disable — None clamps to the minimum (128).
        assert_eq!(tc["thinkingBudget"], 128);
        // includeThoughts true because budget != 0.
        assert_eq!(tc["includeThoughts"], true);
    }

    /// Step 5c: gemini-2.5-flash CAN disable. `--reasoning none` → 0.
    #[test]
    fn build_request_body_thinking_budget_zero_for_none_on_gemini_2_5_flash() {
        let adapter = GeminiAdapter::new(
            "test-key".to_string(),
            "gemini-2.5-flash".to_string(),
            "https://generativelanguage.googleapis.com/v1beta".to_string(),
        )
        .expect("adapter constructs");
        let messages = vec![ChatMessage::user("hi")];
        let config = ModelConfig {
            reasoning: ReasoningLevel::None,
            ..Default::default()
        };
        let body = adapter.build_request_body(&messages, &config);
        let tc = &body["generationConfig"]["thinkingConfig"];
        assert_eq!(tc["thinkingBudget"], 0);
        assert_eq!(tc["includeThoughts"], false);
    }

    /// Step 5c: gemini-2.5-flash with Max → -1 (adaptive sentinel).
    #[test]
    fn build_request_body_thinking_budget_adaptive_for_max_on_gemini_2_5_flash() {
        let adapter = GeminiAdapter::new(
            "test-key".to_string(),
            "gemini-2.5-flash".to_string(),
            "https://generativelanguage.googleapis.com/v1beta".to_string(),
        )
        .expect("adapter constructs");
        let messages = vec![ChatMessage::user("hi")];
        let config = ModelConfig {
            reasoning: ReasoningLevel::Max,
            ..Default::default()
        };
        let body = adapter.build_request_body(&messages, &config);
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            -1
        );
    }

    /// Step 5c: legacy Gemini models (2.0, 1.5) don't support
    /// thinkingConfig — sending one would 400 with a syntax error per
    /// the official docs. Adapter must omit the field entirely.
    #[test]
    fn build_request_body_omits_thinking_config_on_gemini_2_0() {
        let adapter = GeminiAdapter::new(
            "test-key".to_string(),
            "gemini-2.0-flash".to_string(),
            "https://generativelanguage.googleapis.com/v1beta".to_string(),
        )
        .expect("adapter constructs");
        let messages = vec![ChatMessage::user("hi")];
        let config = ModelConfig {
            reasoning: ReasoningLevel::Medium,
            ..Default::default()
        };
        let body = adapter.build_request_body(&messages, &config);
        // No thinkingConfig field on legacy models — would 400 if present.
        assert!(
            body["generationConfig"].get("thinkingConfig").is_none(),
            "legacy Gemini models must NOT receive thinkingConfig"
        );
    }

    // --- gemini_thinking_dispatch ---

    #[test]
    fn dispatch_is_level_for_gemini_3_models() {
        assert_eq!(
            gemini_thinking_dispatch("gemini-3-pro"),
            GeminiThinkingDispatch::Level
        );
        assert_eq!(
            gemini_thinking_dispatch("gemini-3-flash"),
            GeminiThinkingDispatch::Level
        );
        assert_eq!(
            gemini_thinking_dispatch("gemini-3-flash-lite"),
            GeminiThinkingDispatch::Level
        );
    }

    #[test]
    fn dispatch_is_budget_with_min_128_no_disable_for_gemini_2_5_pro() {
        assert_eq!(
            gemini_thinking_dispatch("gemini-2.5-pro"),
            GeminiThinkingDispatch::Budget {
                min: 128,
                can_disable: false
            }
        );
    }

    #[test]
    fn dispatch_is_budget_with_min_512_can_disable_for_gemini_2_5_flash_lite() {
        assert_eq!(
            gemini_thinking_dispatch("gemini-2.5-flash-lite"),
            GeminiThinkingDispatch::Budget {
                min: 512,
                can_disable: true
            }
        );
    }

    #[test]
    fn dispatch_is_budget_with_min_0_can_disable_for_gemini_2_5_flash() {
        assert_eq!(
            gemini_thinking_dispatch("gemini-2.5-flash"),
            GeminiThinkingDispatch::Budget {
                min: 0,
                can_disable: true
            }
        );
    }

    #[test]
    fn dispatch_is_disabled_for_legacy_gemini_models() {
        assert_eq!(
            gemini_thinking_dispatch("gemini-2.0-flash"),
            GeminiThinkingDispatch::Disabled
        );
        assert_eq!(
            gemini_thinking_dispatch("gemini-1.5-pro"),
            GeminiThinkingDispatch::Disabled
        );
    }

    // --- thinking_level_for ---

    #[test]
    fn thinking_level_per_reasoning_level() {
        assert_eq!(thinking_level_for(ReasoningLevel::None), "minimal");
        assert_eq!(thinking_level_for(ReasoningLevel::Minimal), "minimal");
        assert_eq!(thinking_level_for(ReasoningLevel::Low), "low");
        assert_eq!(thinking_level_for(ReasoningLevel::Medium), "medium");
        assert_eq!(thinking_level_for(ReasoningLevel::High), "high");
        // No `max` or `xhigh` tier on Gemini 3 — both collapse to high.
        assert_eq!(thinking_level_for(ReasoningLevel::Max), "high");
        assert_eq!(thinking_level_for(ReasoningLevel::XHigh), "high");
    }

    #[test]
    fn thinking_budget_for_xhigh_matches_max_adaptive_sentinel() {
        // Gemini 2.5 has no xhigh tier — both Max and XHigh map to the
        // adaptive-thinking sentinel value `-1`.
        assert_eq!(thinking_budget_for(ReasoningLevel::Max), -1);
        assert_eq!(thinking_budget_for(ReasoningLevel::XHigh), -1);
    }

    #[test]
    fn build_request_body_includes_tools_in_function_declarations_shape() {
        let adapter = test_adapter();
        let messages = vec![ChatMessage::user("hi")];
        // v7: config carries OpenAI-shape tools populated by the
        // provider wrapper; adapter translates to Gemini's
        // functionDeclarations shape.
        let config = ModelConfig {
            tools: (0..5)
                .map(|i| {
                    serde_json::json!({
                        "type": "function",
                        "function": {
                            "name": format!("tool_{}", i),
                            "description": "a test tool",
                            "parameters": {"type": "object"}
                        }
                    })
                })
                .collect(),
            ..Default::default()
        };
        let body = adapter.build_request_body(&messages, &config);
        let tools = body["tools"].as_array().expect("tools array");
        assert!(!tools.is_empty());
        assert!(tools[0]["functionDeclarations"].is_array());
        let decls = tools[0]["functionDeclarations"].as_array().unwrap();
        assert_eq!(decls.len(), 5);
    }

    #[test]
    fn build_request_body_clamps_temperature() {
        let adapter = test_adapter();
        let messages = vec![ChatMessage::user("hi")];
        let config = ModelConfig {
            temperature: 5.0, // Out-of-range
            ..Default::default()
        };
        let body = adapter.build_request_body(&messages, &config);
        let temp = body["generationConfig"]["temperature"].as_f64().unwrap();
        assert!(temp <= 2.0);
    }

    // --- streaming state machine ---

    use std::sync::Arc;
    use std::sync::Mutex;

    /// Build a callback that records every emitted StreamEvent into a
    /// shared Vec for test assertions.
    fn record_callback() -> (StreamCallback, Arc<Mutex<Vec<StreamEvent>>>) {
        let events: Arc<Mutex<Vec<StreamEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let clone = Arc::clone(&events);
        let cb: StreamCallback = Arc::new(move |evt| {
            clone.lock().unwrap().push(evt);
        });
        (cb, events)
    }

    fn count_text(events: &[StreamEvent]) -> usize {
        events
            .iter()
            .filter(|e| matches!(e, StreamEvent::Text(_)))
            .count()
    }

    fn count_reasoning(events: &[StreamEvent]) -> usize {
        events
            .iter()
            .filter(|e| matches!(e, StreamEvent::Reasoning(_)))
            .count()
    }

    fn count_tool_calls(events: &[StreamEvent]) -> usize {
        events
            .iter()
            .filter(|e| matches!(e, StreamEvent::ToolCall(_)))
            .count()
    }

    #[test]
    fn stream_text_only_multi_chunk() {
        let (cb, events) = record_callback();
        let mut state = StreamState::default();

        // Chunk 1: "Hello, "
        let chunk1 = json!({
            "candidates": [{
                "content": {"parts": [{"text": "Hello, "}]}
            }]
        })
        .to_string();
        process_chunk_payload(&chunk1, &mut state, &cb, false).unwrap();

        // Chunk 2: "world!" + usage.
        let chunk2 = json!({
            "candidates": [{
                "content": {"parts": [{"text": "world!"}]}
            }],
            "usageMetadata": {
                "promptTokenCount": 5,
                "candidatesTokenCount": 3,
                "totalTokenCount": 8
            }
        })
        .to_string();
        process_chunk_payload(&chunk2, &mut state, &cb, false).unwrap();

        assert_eq!(state.text_acc, "Hello, world!");
        assert_eq!(state.prompt_tokens, 5);
        assert_eq!(state.completion_tokens, 3);
        assert_eq!(state.total_tokens, 8);

        let evts = events.lock().unwrap();
        assert_eq!(count_text(&evts), 2);
        assert_eq!(count_reasoning(&evts), 0);
        assert_eq!(count_tool_calls(&evts), 0);
    }

    #[test]
    fn stream_usage_is_none_without_usage_metadata() {
        // #125: a stream that never carried a `usageMetadata` block yields None,
        // so the reducer keeps its char/4 estimate instead of resetting to zero.
        let (cb, _events) = record_callback();
        let mut state = StreamState::default();
        let chunk =
            json!({ "candidates": [{ "content": {"parts": [{"text": "hi"}]} }] }).to_string();
        process_chunk_payload(&chunk, &mut state, &cb, false).unwrap();
        assert!(!state.saw_usage);
        assert!(state.usage(0).is_none());
    }

    #[test]
    fn stream_usage_does_not_double_count_cached_input() {
        // #137: Gemini folds cached tokens into promptTokenCount; the input
        // breakdown must not add them a second time.
        let (cb, _events) = record_callback();
        let mut state = StreamState::default();
        let chunk = json!({
            "candidates": [{ "content": {"parts": [{"text": "hi"}]} }],
            "usageMetadata": {
                "promptTokenCount": 1000,
                "cachedContentTokenCount": 300,
                "candidatesTokenCount": 50,
                "totalTokenCount": 1050
            }
        })
        .to_string();
        process_chunk_payload(&chunk, &mut state, &cb, false).unwrap();
        assert!(state.saw_usage);
        let usage = state.usage(1050).expect("usage present");
        assert_eq!(
            usage.input_total_tokens(),
            1000,
            "cached input must not be double-counted"
        );
    }

    #[test]
    fn stream_thought_then_text() {
        let (cb, events) = record_callback();
        let mut state = StreamState::default();

        let chunk1 = json!({
            "candidates": [{
                "content": {"parts": [{"text": "let me think...", "thought": true}]}
            }]
        })
        .to_string();
        process_chunk_payload(&chunk1, &mut state, &cb, false).unwrap();

        let chunk2 = json!({
            "candidates": [{
                "content": {"parts": [{"text": "the answer is 42"}]}
            }]
        })
        .to_string();
        process_chunk_payload(&chunk2, &mut state, &cb, false).unwrap();

        assert_eq!(state.thinking_acc, "let me think...");
        assert_eq!(state.text_acc, "the answer is 42");

        let evts = events.lock().unwrap();
        assert_eq!(count_reasoning(&evts), 1);
        assert_eq!(count_text(&evts), 1);
    }

    #[test]
    fn stream_function_call_emits_tool_call_event() {
        let (cb, events) = record_callback();
        let mut state = StreamState::default();

        let chunk = json!({
            "candidates": [{
                "content": {
                    "parts": [
                        {"functionCall": {"name": "read_file", "args": {"path": "Cargo.toml"}}}
                    ]
                }
            }]
        })
        .to_string();
        process_chunk_payload(&chunk, &mut state, &cb, false).unwrap();

        assert_eq!(state.tool_calls_done.len(), 1);
        let tc = &state.tool_calls_done[0];
        assert_eq!(tc.function.name, "read_file");
        assert_eq!(tc.function.arguments["path"], "Cargo.toml");
        assert_eq!(tc.id.as_deref(), Some("call_0"));

        let evts = events.lock().unwrap();
        assert_eq!(count_tool_calls(&evts), 1);
    }

    #[test]
    fn stream_thought_text_and_tool_call_in_one_chunk() {
        let (cb, events) = record_callback();
        let mut state = StreamState::default();

        let chunk = json!({
            "candidates": [{
                "content": {
                    "parts": [
                        {"text": "thinking...", "thought": true},
                        {"text": "calling tool now"},
                        {"functionCall": {"name": "list_dir", "args": {"path": "."}}}
                    ]
                }
            }]
        })
        .to_string();
        process_chunk_payload(&chunk, &mut state, &cb, false).unwrap();

        assert_eq!(state.thinking_acc, "thinking...");
        assert_eq!(state.text_acc, "calling tool now");
        assert_eq!(state.tool_calls_done.len(), 1);

        let evts = events.lock().unwrap();
        assert_eq!(count_reasoning(&evts), 1);
        assert_eq!(count_text(&evts), 1);
        assert_eq!(count_tool_calls(&evts), 1);
    }

    #[test]
    fn stream_hide_reasoning_trace_suppresses_event_but_accumulates() {
        let (cb, events) = record_callback();
        let mut state = StreamState::default();

        let chunk = json!({
            "candidates": [{
                "content": {"parts": [{"text": "hidden thoughts", "thought": true}]}
            }]
        })
        .to_string();
        // hide_reasoning_trace = true.
        process_chunk_payload(&chunk, &mut state, &cb, true).unwrap();

        // Accumulator gets the text (so the final ModelResponse.thinking
        // is populated), but no Reasoning event is emitted.
        assert_eq!(state.thinking_acc, "hidden thoughts");
        let evts = events.lock().unwrap();
        assert_eq!(count_reasoning(&evts), 0);
    }

    #[test]
    fn stream_mid_stream_error_returns_error() {
        let (cb, _events) = record_callback();
        let mut state = StreamState::default();

        let chunk = json!({
            "error": {
                "code": 429,
                "message": "Resource exhausted",
                "status": "RESOURCE_EXHAUSTED"
            }
        })
        .to_string();
        let result = process_chunk_payload(&chunk, &mut state, &cb, false);
        assert!(result.is_err());
        match result {
            Err(ModelError::Backend(BackendError::ProviderError { code, message, .. })) => {
                assert_eq!(code.as_deref(), Some("RESOURCE_EXHAUSTED"));
                assert!(message.contains("Resource exhausted"));
            },
            other => panic!("expected ProviderError, got {:?}", other),
        }
    }

    #[test]
    fn stream_tool_call_ids_are_synthesized_in_sequence() {
        let (cb, _events) = record_callback();
        let mut state = StreamState::default();

        let chunk = json!({
            "candidates": [{
                "content": {
                    "parts": [
                        {"functionCall": {"name": "tool_a", "args": {}}},
                        {"functionCall": {"name": "tool_b", "args": {}}}
                    ]
                }
            }]
        })
        .to_string();
        process_chunk_payload(&chunk, &mut state, &cb, false).unwrap();

        assert_eq!(state.tool_calls_done.len(), 2);
        assert_eq!(state.tool_calls_done[0].id.as_deref(), Some("call_0"));
        assert_eq!(state.tool_calls_done[1].id.as_deref(), Some("call_1"));
    }

    #[test]
    fn stream_safety_block_with_no_parts_errors() {
        let (cb, _events) = record_callback();
        let mut state = StreamState::default();
        // A content-free SAFETY block must error, not silently succeed (#1).
        let chunk = json!({ "candidates": [{ "finishReason": "SAFETY" }] }).to_string();
        assert!(process_chunk_payload(&chunk, &mut state, &cb, false).is_err());
    }

    #[test]
    fn stream_max_tokens_keeps_partial_and_sets_length() {
        let (cb, _events) = record_callback();
        let mut state = StreamState::default();
        let chunk = json!({
            "candidates": [{
                "content": {"parts": [{"text": "partial"}]},
                "finishReason": "MAX_TOKENS"
            }]
        })
        .to_string();
        process_chunk_payload(&chunk, &mut state, &cb, false).unwrap();
        assert_eq!(state.finish_reason, Some(FinishReason::Length));
        assert_eq!(state.text_acc, "partial");
    }

    #[test]
    fn stream_closed_abnormally_when_no_finish_reason_observed() {
        // F56: content chunks with no finishReason leave the stream incomplete
        // until a terminal finishReason lands. A drop here must surface as an
        // error, not a clean Ok indistinguishable from a real completion.
        let (cb, _events) = record_callback();
        let mut state = StreamState::default();
        let chunk =
            json!({ "candidates": [{ "content": {"parts": [{"text": "partial answer"}]} }] })
                .to_string();
        process_chunk_payload(&chunk, &mut state, &cb, false).unwrap();
        assert!(
            stream_closed_abnormally(state.finish_reason.as_ref()),
            "no finishReason observed yet → abnormal if the stream ends here"
        );

        // The terminal chunk carrying STOP completes it.
        let final_chunk = json!({
            "candidates": [{ "content": {"parts": [{"text": "."}]}, "finishReason": "STOP" }]
        })
        .to_string();
        process_chunk_payload(&final_chunk, &mut state, &cb, false).unwrap();
        assert!(!stream_closed_abnormally(state.finish_reason.as_ref()));
    }

    #[test]
    fn stream_closed_abnormally_preserves_max_tokens_truncation() {
        // CRUCIAL: a MAX_TOKENS truncation is a real terminal finishReason
        // (Length) — it must NOT be misclassified as an abnormal close.
        assert!(stream_closed_abnormally(None));
        assert!(!stream_closed_abnormally(Some(&FinishReason::Length)));
        assert!(!stream_closed_abnormally(Some(&FinishReason::Stop)));
    }

    #[test]
    fn parallel_same_tool_calls_keep_call_order() {
        // #8: Gemini's wire protocol has no call id, so two calls to the SAME
        // tool in one turn are associated by POSITION. We synthesize ids in
        // arrival order and must emit results in the same order — that ordering
        // is the only correctness lever, so pin it against accidental reordering.
        let (cb, _events) = record_callback();
        let mut state = StreamState::default();
        let chunk = json!({
            "candidates": [{
                "content": {"parts": [
                    {"functionCall": {"name": "read_file", "args": {"path": "a"}}},
                    {"functionCall": {"name": "read_file", "args": {"path": "b"}}}
                ]}
            }]
        })
        .to_string();
        process_chunk_payload(&chunk, &mut state, &cb, false).unwrap();
        assert_eq!(state.tool_calls_done.len(), 2);
        assert_eq!(state.tool_calls_done[0].id.as_deref(), Some("call_0"));
        assert_eq!(state.tool_calls_done[1].id.as_deref(), Some("call_1"));
        assert_eq!(state.tool_calls_done[0].function.arguments["path"], "a");
        assert_eq!(state.tool_calls_done[1].function.arguments["path"], "b");
    }
}
