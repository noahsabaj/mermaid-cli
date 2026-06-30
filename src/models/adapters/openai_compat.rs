//! OpenAI-compatible Chat Completions adapter.
//!
//! Single adapter that targets `POST /chat/completions` (the universal
//! shape across OpenAI itself and ~10 conformant providers — Groq,
//! Together, Fireworks, OpenRouter, vLLM, DeepInfra, Cerebras,
//! SambaNova, LMStudio, llama.cpp). Provider-specific quirks live in
//! `ProviderProfile` (`crate::models::providers`) — the adapter asks the
//! profile how to render reasoning depth and where to find reasoning
//! content, and otherwise treats every provider identically.
//!
//! Streaming uses SSE (`data: <json>\n\n` ... `data: [DONE]\n\n`),
//! drained via `crate::utils::drain_sse_events`. Tool calls arrive as
//! chunked deltas indexed by `tool_calls[].index` and accumulated
//! locally. Reasoning content arrives in either a named delta field
//! (`delta.reasoning_content` for vLLM/DeepInfra/DeepSeek, `delta.reasoning`
//! for Groq parsed mode + OpenRouter), inline `<think>...</think>`
//! tags inside `delta.content` (Together-R1, Wave 6 adds the stripper),
//! or not at all (OpenAI Chat Completions encrypts).
//!
//! # Why Chat Completions, not Responses API
//!
//! As of 2026-04, OpenAI's official docs flag the Responses API
//! (`POST /responses`) as the recommended default and Chat Completions
//! (`POST /chat/completions`) as legacy. Mermaid uses Chat Completions
//! deliberately because it's the universal OpenAI-compat shape: Groq,
//! OpenRouter, Cerebras, DeepInfra, Together, Fireworks, vLLM, and
//! SambaNova all implement Chat Completions; the Responses API is
//! OpenAI-only. Migrating this adapter would either (a) break OpenAI-
//! compat coverage for those providers, or (b) require a separate
//! OpenAI-direct adapter that bypasses this path. Both are non-trivial
//! work for marginal gain — Chat Completions still works on the OpenAI
//! direct endpoint, just without Responses-specific features (built-in
//! reasoning summaries, structured-output tools, etc.).
//!
//! When/if a Responses-only feature becomes load-bearing for Mermaid,
//! the right move is a focused new adapter (`openai_responses.rs`)
//! routed through `providers::factory::ProviderFactory` for `provider == "openai"`,
//! leaving this OpenAI-compat path for everyone else.

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::constants::MAX_RESPONSE_CHARS;
use crate::models::ModelCapabilities;
use crate::models::config::ModelConfig;
use crate::models::error::{BackendError, ModelError, Result};
use crate::models::providers::{
    MaxTokensParam, ProviderProfile, ReasoningExtraction, ReasoningStrategy,
};
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
/// shape as the helper in `adapters/ollama.rs` — duplicated rather than
/// shared because (a) the marker text differs in spirit (provider-specific
/// limits could grow different copy later), and (b) the dependency
/// graph stays one-way (utils have no provider knowledge).
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
/// `arguments` fragments and grow this buffer without limit (the daemon is
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

/// Map OpenAI's `finish_reason` onto the normalized [`FinishReason`].
fn map_openai_finish_reason(s: &str) -> FinishReason {
    match s {
        "stop" => FinishReason::Stop,
        "length" => FinishReason::Length,
        "tool_calls" | "function_call" => FinishReason::ToolUse,
        "content_filter" => FinishReason::ContentFilter,
        other => FinishReason::Other(other.to_string()),
    }
}

/// OpenAI reasoning models (the `o1`/`o3`/`o4` families and `gpt-5`) reject any
/// non-default `temperature` with a 400, so the request builder omits it for
/// them (#124). Matches the bare model id (any `provider/` prefix stripped).
fn is_reasoning_model(model_name: &str) -> bool {
    let m = model_name.rsplit('/').next().unwrap_or(model_name);
    m.starts_with("o1") || m.starts_with("o3") || m.starts_with("o4") || m.starts_with("gpt-5")
}

/// OpenAI-compatible model adapter.
///
/// Constructed via `OpenAICompatAdapter::new` from `providers::factory::ProviderFactory` once the
/// provider name has been resolved against the registry / user config.
/// All fields are owned (not borrowed) so the adapter outlives the
/// factory call that built it.
pub struct OpenAICompatAdapter {
    client: Client,
    profile: &'static ProviderProfile,
    base_url: String,
    api_key: String,
    model_name: String,
    /// Includes both the profile's `extra_headers` and any user overrides.
    extra_headers: HashMap<String, String>,
    capabilities: ModelCapabilities,
}

/// A random 128-bit `Idempotency-Key`, hex-encoded, for safe retry de-duplication
/// (#F27). On the (vanishingly rare) OS-RNG failure, fall back to a
/// process+time value so we still send *a* stable key rather than none.
fn random_idempotency_key() -> String {
    let mut bytes = [0u8; 16];
    if getrandom::fill(&mut bytes).is_err() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        return format!("mermaid-{}-{nanos}", std::process::id());
    }
    use std::fmt::Write;
    bytes.iter().fold(String::with_capacity(32), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

impl OpenAICompatAdapter {
    /// Create a new adapter. `base_url` is the resolved URL (registry
    /// default OR user override); `api_key` is already resolved (caller
    /// uses `crate::utils::resolve_api_key`).
    pub fn new(
        profile: &'static ProviderProfile,
        base_url: String,
        api_key: String,
        model_name: String,
        extra_headers: HashMap<String, String>,
    ) -> Result<Self> {
        // Same client config as the Ollama adapter: connection-pooled,
        // long-lived idle, no global request timeout (streaming responses
        // can take minutes for large contexts).
        let client = Client::builder()
            .pool_max_idle_per_host(10)
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(60))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| {
                ModelError::Backend(BackendError::ConnectionFailed {
                    backend: profile.name.to_string(),
                    url: base_url.clone(),
                    reason: e.to_string(),
                })
            })?;

        let capabilities = derive_capabilities(profile);

        Ok(Self {
            client,
            profile,
            base_url,
            api_key,
            model_name,
            extra_headers,
            capabilities,
        })
    }

    /// Build the JSON request body for `/chat/completions`. Shared
    /// between streaming and non-streaming paths.
    fn build_request_body(
        &self,
        messages: &[ChatMessage],
        config: &ModelConfig,
        stream: bool,
    ) -> Value {
        let mut json_messages = Vec::new();

        // Step 5h: combined_system_prompt joins the static base with
        // any MERMAID.md content (separator `---`). On OpenAI-compat
        // we have no per-block cache markers, so this is the right
        // shape — the model just sees one extended system message.
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
            // OpenAI tool result messages: `role: "tool"`, `tool_call_id`,
            // and `name` (the tool name). Identical to Ollama's shape
            // except the field is `name`, not `tool_name`.
            if msg.role == MessageRole::Tool {
                if let Some(ref tool_call_id) = msg.tool_call_id {
                    json_msg["tool_call_id"] = json!(tool_call_id);
                }
                if let Some(ref tool_name) = msg.tool_name {
                    json_msg["name"] = json!(tool_name);
                }
            }
            if let Some(ref images) = msg.images
                && !images.is_empty()
            {
                // OpenAI vision shape uses `content` as an array of
                // typed parts. Step 2 doesn't ship vision (no provider
                // in the v1 registry advertises it); skip silently.
                let _ = images;
            }
            json_messages.push(json_msg);
        }

        // Tools come from `config.tools` (OpenAI-compat shape is the
        // canonical one we pass around; the Anthropic / Gemini
        // adapters translate from it). Drop web tools without a
        // cloud API key.
        let no_cloud_key = crate::ollama::get_cloud_api_key().is_none();
        let tools: Vec<&Value> = config
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

        let mut body = json!({
            "model": self.model_name,
            "messages": json_messages,
            "stream": stream,
        });
        // OpenAI o-series / gpt-5 reasoning models reject any non-default
        // `temperature` with a 400, so it's sent only for models that accept it
        // (#124). Clamp to the accepted 0..=2 (a stale config value otherwise 400s).
        if !is_reasoning_model(&self.model_name) {
            body["temperature"] = json!(config.temperature.clamp(0.0, 2.0));
        }

        if stream {
            body["stream_options"] = json!({ "include_usage": true });
        }

        if !tools.is_empty() {
            body["tools"] = json!(tools);
            if self
                .profile
                .disable_parallel_tool_calls_for
                .contains(&self.model_name.as_str())
            {
                body["parallel_tool_calls"] = json!(false);
            }
        }

        // Completion budget spelling is provider-specific even inside the
        // OpenAI-compatible family.
        if config.max_tokens > 0 {
            match self.profile.max_tokens_param {
                MaxTokensParam::MaxTokens => body["max_tokens"] = json!(config.max_tokens),
                MaxTokensParam::MaxCompletionTokens => {
                    body["max_completion_tokens"] = json!(config.max_tokens);
                },
            }
        }

        // Reasoning depth: snap the requested level onto what the model
        // actually supports (`nearest_effort`), then ask the profile what
        // to splice in. Snap is a defensive guard — `Effort` and
        // `OpenRouterShape` strategies currently advertise the full enum,
        // but a future per-model capability shrink (e.g. a hypothetical
        // `gpt-mini` exposing only Low/Medium) would land cleanly without
        // touching the request-body builder.
        let effective_reasoning = match &self.capabilities.supports_reasoning {
            ReasoningCapability::Levels(supported) => {
                nearest_effort(config.reasoning, supported).unwrap_or(ReasoningLevel::None)
            },
            _ => config.reasoning,
        };
        if let Some(reasoning_value) = self.profile.reasoning_strategy.render(effective_reasoning) {
            // The strategy returns a one-key object; merge its top-level
            // entries into the request body.
            if let Some(obj) = reasoning_value.as_object() {
                for (k, v) in obj {
                    body[k] = v.clone();
                }
            }
        }

        body
    }

    /// POST `/chat/completions` and return the raw response.
    /// Transparently retries on 5xx, 429, or reqwest connect failures
    /// via `crate::effect::retry_transient_http`. Useful for Groq /
    /// OpenRouter / etc. when an upstream relay hiccups.
    async fn send_chat(&self, body: &Value) -> Result<reqwest::Response> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        // A stable idempotency key, generated ONCE and reused across every retry
        // attempt, lets an OpenAI-compatible endpoint that honors `Idempotency-Key`
        // (OpenAI, Groq, OpenRouter, …) dedupe a retried POST instead of generating
        // — and billing — a second completion when a transient 5xx/connection drop
        // is retried after the server already produced one (#F27). Endpoints that
        // ignore the header are unaffected. (Anthropic has no documented
        // equivalent; its retries mirror the official SDK default.)
        let idempotency_key = random_idempotency_key();
        crate::effect::retry_transient_http(|| async {
            let mut req = self
                .client
                .post(&url)
                .bearer_auth(&self.api_key)
                .header("Idempotency-Key", &idempotency_key)
                .json(body);
            for (name, value) in &self.extra_headers {
                req = req.header(name, value);
            }
            req.send().await.map_err(|e| {
                ModelError::Backend(BackendError::ConnectionFailed {
                    backend: self.profile.name.to_string(),
                    url: url.clone(),
                    reason: e.to_string(),
                })
            })
        })
        .await
    }

    /// Decode a single non-streaming response into `ModelResponse`.
    async fn decode_non_streaming(&self, response: reqwest::Response) -> Result<ModelResponse> {
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(ModelError::Backend(BackendError::HttpError {
                status,
                message: body,
            }));
        }
        let json: ChatCompletion = response.json().await.map_err(|e| ModelError::ParseError {
            message: format!("Failed to parse {} response: {}", self.profile.name, e),
            raw: None,
        })?;

        let choice = json
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| ModelError::ParseError {
                message: format!("{} response had no choices", self.profile.name),
                raw: None,
            })?;

        let usage = json.usage.map(token_usage_from_wire);

        // Reasoning content: extract from the named field if the profile
        // points at one. For `InlineThinkTags`, leave it in `content`
        // for now — Wave 6 wires the stripper.
        // For InlineThinkTags providers the non-streaming body still contains
        // `<think>…</think>`; run it through the same stripper the streaming path
        // uses so reasoning is separated out of `content` (#5).
        let raw_content = choice.message.content.unwrap_or_default();
        let (content, inline_thinking) = match self.profile.reasoning_extraction {
            ReasoningExtraction::InlineThinkTags => {
                let mut ts = ThinkTagState::new();
                let (mut text, mut reasoning) = ts.feed(&raw_content);
                let (text_tail, reasoning_tail) = ts.flush();
                text.push_str(&text_tail);
                reasoning.push_str(&reasoning_tail);
                (text, (!reasoning.is_empty()).then_some(reasoning))
            },
            _ => (raw_content, None),
        };

        let thinking = match self.profile.reasoning_extraction {
            ReasoningExtraction::DeltaContentField(field) => choice
                .message
                .extra
                .get(field)
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .filter(|s| !s.is_empty()),
            ReasoningExtraction::InlineThinkTags => inline_thinking,
            _ => None,
        };

        let tool_calls = choice
            .message
            .tool_calls
            .filter(|v| !v.is_empty())
            .map(|raw| raw.into_iter().filter_map(parse_full_tool_call).collect());

        let stop_reason = choice
            .finish_reason
            .as_deref()
            .map(map_openai_finish_reason);
        if content.is_empty()
            && tool_calls.is_none()
            && stop_reason == Some(FinishReason::ContentFilter)
        {
            return Err(ModelError::Backend(BackendError::ProviderError {
                provider: self.profile.name.to_string(),
                code: Some("content_filter".to_string()),
                message: "Provider returned no content (content filter)".to_string(),
            }));
        }

        Ok(ModelResponse {
            content,
            usage,
            model_name: self.model_name.clone(),
            stop_reason,
            thinking,
            tool_calls,
            thinking_signature: None,
        })
    }

    /// Stream the response, emit typed events through the callback,
    /// return the final accumulated `ModelResponse`.
    async fn handle_stream(
        &self,
        response: reqwest::Response,
        callback: StreamCallback,
        hide_reasoning_trace: bool,
    ) -> Result<ModelResponse> {
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(ModelError::Backend(BackendError::HttpError {
                status,
                message: body,
            }));
        }

        let mut stream = response.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();

        let mut content_acc = String::new();
        let mut thinking_acc = String::new();
        let mut tool_calls_partial: Vec<PartialToolCall> = Vec::new();
        let mut truncated = false;
        let mut stop_reason: Option<FinishReason> = None;
        // The full token breakdown (cached-input + reasoning) from the last usage
        // frame. Stays `None` until a usage frame arrives, so a stream that never
        // reports usage returns `None` (the reducer then keeps its estimate)
        // rather than a misleading zero (#125).
        let mut usage_acc: Option<TokenUsage> = None;
        // For providers that emit `<think>...</think>` inline in
        // `delta.content`, route the content channel through this state
        // machine so reasoning gets split out into its own
        // `StreamEvent::Reasoning` events.
        let inline_tags = matches!(
            self.profile.reasoning_extraction,
            ReasoningExtraction::InlineThinkTags
        );
        let mut think_state = ThinkTagState::new();

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
                // A mid-stream error frame (common on OpenRouter) is an
                // `{"error": ...}` object, not a chat chunk. Surface it as a
                // typed provider error instead of the confusing "missing field
                // choices" parse failure (#123) — mirrors the Gemini path.
                let value: serde_json::Value = match serde_json::from_str(&payload) {
                    Ok(v) => v,
                    Err(e) => {
                        return Err(ModelError::ParseError {
                            message: format!(
                                "Failed to parse {} stream chunk: {}",
                                self.profile.name, e
                            ),
                            raw: Some(payload),
                        });
                    },
                };
                if let Some(err) = value.get("error") {
                    let code = err.get("code").and_then(|v| {
                        v.as_str()
                            .map(str::to_string)
                            .or_else(|| v.as_i64().map(|n| n.to_string()))
                    });
                    let message = err
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("stream error")
                        .to_string();
                    return Err(ModelError::Backend(BackendError::ProviderError {
                        provider: self.profile.name.to_string(),
                        code,
                        message,
                    }));
                }
                let parsed: ChatCompletionChunk = match serde_json::from_value(value) {
                    Ok(v) => v,
                    Err(e) => {
                        return Err(ModelError::ParseError {
                            message: format!(
                                "Failed to parse {} stream chunk: {}",
                                self.profile.name, e
                            ),
                            raw: Some(payload),
                        });
                    },
                };

                if let Some(usage) = parsed.usage {
                    // #12: capture the cached-input + reasoning breakdown via the
                    // same converter the non-stream path uses. The last usage
                    // frame wins.
                    usage_acc = Some(token_usage_from_wire(usage));
                }

                let Some(choice) = parsed.choices.into_iter().next() else {
                    continue;
                };

                if let Some(fr) = &choice.finish_reason {
                    stop_reason = Some(map_openai_finish_reason(fr));
                }
                let delta = choice.delta;

                // Reasoning extraction (separate field). InlineThinkTags
                // is handled at the byte-stream level via Wave 6's state
                // machine; it returns None here.
                let reasoning_chunk = match self.profile.reasoning_extraction {
                    ReasoningExtraction::DeltaContentField(field) => delta
                        .extra
                        .get(field)
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .map(|s| ReasoningChunk {
                            text: s.to_string(),
                            signature: None,
                        }),
                    _ => None,
                };
                if let Some(chunk) = reasoning_chunk {
                    if !hide_reasoning_trace {
                        callback(StreamEvent::Reasoning(chunk.clone()));
                    }
                    push_capped(
                        &mut thinking_acc,
                        &chunk.text,
                        &mut truncated,
                        MAX_RESPONSE_CHARS,
                    );
                }

                // Text content. For inline-tags providers, route through
                // the ThinkTagState machine which splits out reasoning
                // into its own channel; otherwise emit as plain text.
                if let Some(text) = delta.content.as_ref()
                    && !text.is_empty()
                    && !truncated
                {
                    if inline_tags {
                        let (text_part, reasoning_part) = think_state.feed(text);
                        if !text_part.is_empty() {
                            callback(StreamEvent::Text(text_part.clone()));
                            push_capped(
                                &mut content_acc,
                                &text_part,
                                &mut truncated,
                                MAX_RESPONSE_CHARS,
                            );
                        }
                        if !reasoning_part.is_empty() {
                            if !hide_reasoning_trace {
                                callback(StreamEvent::Reasoning(ReasoningChunk {
                                    text: reasoning_part.clone(),
                                    signature: None,
                                }));
                            }
                            push_capped(
                                &mut thinking_acc,
                                &reasoning_part,
                                &mut truncated,
                                MAX_RESPONSE_CHARS,
                            );
                        }
                    } else {
                        callback(StreamEvent::Text(text.clone()));
                        push_capped(&mut content_acc, text, &mut truncated, MAX_RESPONSE_CHARS);
                    }
                }

                // Tool-call deltas — accumulate into partials.
                if let Some(deltas) = delta.tool_calls {
                    for tc_delta in deltas {
                        accumulate_tool_call(&mut tool_calls_partial, tc_delta);
                    }
                }
            }
        }

        // Flush any pending tag-state bytes (incomplete trailing tags
        // get emitted to the text channel; see ThinkTagState::flush).
        if inline_tags {
            let (text_tail, reasoning_tail) = think_state.flush();
            if !text_tail.is_empty() && !truncated {
                callback(StreamEvent::Text(text_tail.clone()));
                push_capped(
                    &mut content_acc,
                    &text_tail,
                    &mut truncated,
                    MAX_RESPONSE_CHARS,
                );
            }
            if !reasoning_tail.is_empty() && !truncated {
                if !hide_reasoning_trace {
                    callback(StreamEvent::Reasoning(ReasoningChunk {
                        text: reasoning_tail.clone(),
                        signature: None,
                    }));
                }
                push_capped(
                    &mut thinking_acc,
                    &reasoning_tail,
                    &mut truncated,
                    MAX_RESPONSE_CHARS,
                );
            }
        }

        // Finalize accumulated tool calls — parse arguments JSON, emit
        // ToolCall events, build the response field.
        let mut final_tool_calls: Vec<ToolCall> = Vec::new();
        for partial in tool_calls_partial {
            if let Some(tc) = partial.into_tool_call() {
                callback(StreamEvent::ToolCall(tc.clone()));
                final_tool_calls.push(tc);
            }
        }

        // F3: wrapper emits the authoritative `Done` from the returned
        // `ModelResponse`. See adapters/anthropic.rs for rationale.

        let thinking = if thinking_acc.is_empty() {
            None
        } else {
            Some(thinking_acc)
        };
        let tool_calls = if final_tool_calls.is_empty() {
            None
        } else {
            Some(final_tool_calls)
        };

        // A content-filter refusal that produced no usable output is an error,
        // not an empty success.
        if content_acc.is_empty()
            && tool_calls.is_none()
            && stop_reason == Some(FinishReason::ContentFilter)
        {
            return Err(ModelError::Backend(BackendError::ProviderError {
                provider: self.profile.name.to_string(),
                code: Some("content_filter".to_string()),
                message: "Provider returned no content (content filter)".to_string(),
            }));
        }

        Ok(ModelResponse {
            content: content_acc,
            // `None` when the stream never reported usage, so the reducer keeps
            // its char/4 estimate instead of resetting the gauge to zero (#125).
            usage: usage_acc,
            model_name: self.model_name.clone(),
            stop_reason,
            thinking,
            tool_calls,
            thinking_signature: None,
        })
    }
}

/// Derive `ModelCapabilities` from a `ProviderProfile`. Reasoning support
/// follows from the strategy:
/// - `Effort` (OpenAI Chat Completions, Groq, Cerebras, Fireworks) advertises
///   the full enum including `Minimal` because OpenAI GPT-5 has a real
///   `minimal` tier and the wire field accepts it. Other models on this
///   strategy that don't honor `minimal` simply ignore the field.
/// - `OpenRouterShape` advertises `[None, Low, Medium, High, Max]` because
///   OpenRouter's normalized object has no `minimal` — `Minimal` requests
///   snap to `Low` via `nearest_effort`.
/// - `None` advertises `Unsupported`.
fn derive_capabilities(profile: &ProviderProfile) -> ModelCapabilities {
    use ReasoningCapability as Cap;
    let supports_reasoning = match profile.reasoning_strategy {
        ReasoningStrategy::None => Cap::Unsupported,
        // Effort providers (OpenAI, Groq, Cerebras, Fireworks, …) accept
        // the full enum on-paper. GPT-5.2+ is the only model that honors
        // `xhigh`; others silently downgrade on the server side.
        ReasoningStrategy::Effort => Cap::Levels(vec![
            ReasoningLevel::None,
            ReasoningLevel::Minimal,
            ReasoningLevel::Low,
            ReasoningLevel::Medium,
            ReasoningLevel::High,
            ReasoningLevel::Max,
            ReasoningLevel::XHigh,
        ]),
        // OpenRouter's normalized object has no `minimal` and no `xhigh`;
        // users who request those snap down via `nearest_effort` (Minimal
        // → None → `{exclude: true}` fallback; XHigh → Max → `{effort: "max"}`).
        ReasoningStrategy::OpenRouterShape => Cap::Levels(vec![
            ReasoningLevel::None,
            ReasoningLevel::Low,
            ReasoningLevel::Medium,
            ReasoningLevel::High,
            ReasoningLevel::Max,
        ]),
    };
    ModelCapabilities {
        supports_tools: true,
        supports_vision: false,
        supports_reasoning,
        max_context_tokens: None,
    }
}

#[async_trait]
impl Model for OpenAICompatAdapter {
    fn name(&self) -> &str {
        &self.model_name
    }

    fn capabilities(&self) -> &ModelCapabilities {
        &self.capabilities
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        let url = format!("{}/models", self.base_url.trim_end_matches('/'));
        let mut req = self.client.get(&url).bearer_auth(&self.api_key);
        for (name, value) in &self.extra_headers {
            req = req.header(name, value);
        }
        let response = req.send().await.map_err(|e| {
            ModelError::Backend(BackendError::ConnectionFailed {
                backend: self.profile.name.to_string(),
                url: url.clone(),
                reason: e.to_string(),
            })
        })?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(ModelError::Unsupported {
                feature: format!("list_models (provider: {})", self.profile.name),
            });
        }
        if !response.status().is_success() {
            return Err(ModelError::Backend(BackendError::HttpError {
                status: response.status().as_u16(),
                message: format!("{} list_models failed", self.profile.name),
            }));
        }
        let body: ListModelsResponse =
            response.json().await.map_err(|e| ModelError::ParseError {
                message: format!("Failed to parse {} models list: {}", self.profile.name, e),
                raw: None,
            })?;
        Ok(body.data.into_iter().map(|m| m.id).collect())
    }

    async fn chat(
        &self,
        messages: &[ChatMessage],
        config: &ModelConfig,
        callback: Option<StreamCallback>,
    ) -> Result<ModelResponse> {
        let stream = callback.is_some();
        let body = self.build_request_body(messages, config, stream);
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

/// Non-streaming `/chat/completions` response.
#[derive(Debug, Deserialize)]
struct ChatCompletion {
    choices: Vec<NonStreamingChoice>,
    #[serde(default)]
    usage: Option<UsageWire>,
}

#[derive(Debug, Deserialize)]
struct NonStreamingChoice {
    message: ResponseMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

/// Non-streaming response message. `extra` captures whatever extra fields
/// (`reasoning_content`, `reasoning`) the provider emits — extracted via
/// `ReasoningExtraction::parse_delta`-like logic in the adapter.
#[derive(Debug, Deserialize)]
struct ResponseMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ToolCallWire>>,
    #[serde(flatten)]
    extra: serde_json::Map<String, Value>,
}

/// Streaming response chunk (one SSE event payload).
#[derive(Debug, Deserialize)]
struct ChatCompletionChunk {
    // A final usage-only frame (and some providers' keep-alives) carry no
    // `choices`; default to empty so it parses instead of 400-ing the stream
    // with "missing field choices" (#123).
    #[serde(default)]
    choices: Vec<StreamingChoice>,
    #[serde(default)]
    usage: Option<UsageWire>,
}

#[derive(Debug, Deserialize)]
struct StreamingChoice {
    #[serde(default)]
    delta: DeltaMessage,
    /// Terminal reason (`stop`/`length`/`tool_calls`/`content_filter`).
    /// Mapped to `FinishReason` so truncation and refusals surface instead of
    /// looking like a clean finish.
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct DeltaMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ToolCallDeltaWire>>,
    /// All other fields (`reasoning_content`, `reasoning`, `role`, etc.)
    /// land here. The adapter uses `extra.get(field)` to pluck reasoning
    /// out per the profile's `ReasoningExtraction` setting.
    #[serde(flatten)]
    extra: serde_json::Map<String, Value>,
}

#[derive(Debug, Deserialize)]
struct UsageWire {
    #[serde(default)]
    prompt_tokens: Option<usize>,
    #[serde(default)]
    completion_tokens: Option<usize>,
    #[serde(default)]
    total_tokens: Option<usize>,
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokensDetailsWire>,
    #[serde(default)]
    completion_tokens_details: Option<CompletionTokensDetailsWire>,
    #[serde(default)]
    input_tokens_details: Option<PromptTokensDetailsWire>,
    #[serde(default)]
    output_tokens_details: Option<CompletionTokensDetailsWire>,
}

#[derive(Debug, Deserialize)]
struct PromptTokensDetailsWire {
    #[serde(default)]
    cached_tokens: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct CompletionTokensDetailsWire {
    #[serde(default)]
    reasoning_tokens: Option<usize>,
}

fn token_usage_from_wire(usage: UsageWire) -> TokenUsage {
    let raw_prompt_tokens = usage.prompt_tokens.unwrap_or(0);
    let completion_tokens = usage.completion_tokens.unwrap_or(0);
    let total_tokens = usage
        .total_tokens
        .unwrap_or_else(|| raw_prompt_tokens.saturating_add(completion_tokens));

    let cached_input_tokens = usage
        .prompt_tokens_details
        .as_ref()
        .and_then(|d| d.cached_tokens)
        .or_else(|| {
            usage
                .input_tokens_details
                .as_ref()
                .and_then(|d| d.cached_tokens)
        })
        .unwrap_or(0);
    // OpenAI's `prompt_tokens` already INCLUDES the cached tokens (a nested
    // breakdown), unlike Anthropic's disjoint buckets. Subtract them so the
    // shared `TokenUsage::input_total_tokens()` (prompt + cached) doesn't
    // double-count the cache hit. `total_tokens` stays the wire value (the
    // billing truth); the fallback above intentionally uses the raw prompt.
    let prompt_tokens = raw_prompt_tokens.saturating_sub(cached_input_tokens);
    let reasoning_output_tokens = usage
        .completion_tokens_details
        .as_ref()
        .and_then(|d| d.reasoning_tokens)
        .or_else(|| {
            usage
                .output_tokens_details
                .as_ref()
                .and_then(|d| d.reasoning_tokens)
        })
        .unwrap_or(0);

    TokenUsage::provider(prompt_tokens, completion_tokens, total_tokens)
        .with_cached_input(cached_input_tokens)
        .with_reasoning_output(reasoning_output_tokens)
}

/// Full tool call as returned in non-streaming responses.
#[derive(Debug, Deserialize, Serialize, Clone)]
struct ToolCallWire {
    #[serde(default)]
    id: Option<String>,
    #[serde(rename = "type", default)]
    #[allow(dead_code)]
    type_: Option<String>,
    function: FunctionWire,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct FunctionWire {
    name: String,
    /// OpenAI emits `arguments` as a JSON-encoded string (not an object).
    /// We parse it lazily into `serde_json::Value` when constructing the
    /// `ToolCall` for the agent loop.
    #[serde(default)]
    arguments: String,
}

/// Streaming tool-call delta. First chunk for a given `index` carries
/// `id` + `function.name`; subsequent chunks append to `function.arguments`
/// fragment-by-fragment.
#[derive(Debug, Deserialize)]
struct ToolCallDeltaWire {
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(rename = "type", default)]
    #[allow(dead_code)]
    type_: Option<String>,
    #[serde(default)]
    function: Option<FunctionDeltaWire>,
}

#[derive(Debug, Deserialize, Default)]
struct FunctionDeltaWire {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

/// Local accumulator for streaming tool calls. Indexed by the wire `index`
/// field; assembled into a `ToolCall` once the stream ends.
#[derive(Debug, Default)]
struct PartialToolCall {
    id: Option<String>,
    name: Option<String>,
    arguments_buf: String,
}

impl PartialToolCall {
    fn into_tool_call(self) -> Option<ToolCall> {
        let name = self.name?;
        // Empty arguments buffer → empty JSON object. OpenAI's contract
        // is that `arguments` is a JSON-encoded string; parse it back.
        let arguments: Value = if self.arguments_buf.is_empty() {
            json!({})
        } else {
            match serde_json::from_str(&self.arguments_buf) {
                Ok(v) => v,
                Err(_) => {
                    // Malformed JSON: surface the raw string so the
                    // executor can decide how to handle it. The agent
                    // loop's parse-error path will catch this.
                    Value::String(self.arguments_buf)
                },
            }
        };
        Some(ToolCall {
            id: self.id,
            function: FunctionCall { name, arguments },
        })
    }
}

fn accumulate_tool_call(partials: &mut Vec<PartialToolCall>, delta: ToolCallDeltaWire) {
    // Bound the stream-controlled index before it drives an allocation. A
    // crafted or buggy upstream could send `index: usize::MAX`, which would
    // otherwise try to grow `partials` by billions of entries and OOM the
    // (long-lived) daemon. No real response has this many parallel calls.
    if delta.index >= crate::constants::MAX_TOOL_CALLS {
        tracing::warn!(
            index = delta.index,
            "dropping tool-call delta with implausible index",
        );
        return;
    }
    while partials.len() <= delta.index {
        partials.push(PartialToolCall::default());
    }
    let slot = &mut partials[delta.index];
    if let Some(id) = delta.id {
        slot.id = Some(id);
    }
    if let Some(func) = delta.function {
        if let Some(name) = func.name {
            slot.name = Some(name);
        }
        if let Some(args) = func.arguments {
            push_tool_arg(&mut slot.arguments_buf, &args);
        }
    }
}

fn parse_full_tool_call(wire: ToolCallWire) -> Option<ToolCall> {
    let name = wire.function.name;
    let arguments: Value = if wire.function.arguments.is_empty() {
        json!({})
    } else {
        match serde_json::from_str(&wire.function.arguments) {
            Ok(v) => v,
            Err(_) => Value::String(wire.function.arguments),
        }
    };
    Some(ToolCall {
        id: wire.id,
        function: FunctionCall { name, arguments },
    })
}

#[derive(Debug, Deserialize)]
struct ListModelsResponse {
    data: Vec<ModelInfo>,
}

#[derive(Debug, Deserialize)]
struct ModelInfo {
    id: String,
}

// ===== Inline <think> tag stripping (Wave 6) =====
//
// Some OpenAI-compatible providers (Together for DeepSeek-R1, Groq in
// `reasoning_format=raw` mode, Fireworks Qwen with `/think` suffixes)
// emit reasoning content as `<think>...</think>` tag pairs inside
// `delta.content` instead of in a separate `delta.reasoning_content`
// field. This state machine consumes content-channel bytes one at a
// time and routes them to either the text channel (outside tags) or the
// reasoning channel (inside tags). Tags can split across SSE chunks
// (`<thi` + `nk>`), so prefix bytes that *could* be the start of a tag
// are buffered until enough data arrives to disambiguate.
//
// Tag matching is case-sensitive on the literal `<think>` and `</think>`
// strings. Other angle-bracketed sequences (`<other>`, `<<`) flow
// through to the text channel unchanged.

const THINK_OPEN: &str = "<think>";
const THINK_CLOSE: &str = "</think>";

#[derive(Debug, Default)]
pub(crate) struct ThinkTagState {
    /// Bytes that could be the start of `<think>` or `</think>` and
    /// haven't been disambiguated yet. Always a prefix of one of those
    /// two strings (max 8 bytes).
    pending: String,
    /// True when we're currently between `<think>` and `</think>`.
    inside: bool,
}

impl ThinkTagState {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk of content text, returning `(text_out, reasoning_out)`.
    /// Either string may be empty.
    pub(crate) fn feed(&mut self, chunk: &str) -> (String, String) {
        let mut text = String::new();
        let mut reasoning = String::new();
        // Prepend any buffered prefix bytes from the previous chunk so
        // we can scan continuously.
        let mut buf = std::mem::take(&mut self.pending);
        buf.push_str(chunk);

        let mut i = 0usize;
        while i < buf.len() {
            // The marker we're hunting for changes based on which side of
            // the tag pair we're currently on.
            let marker = if self.inside { THINK_CLOSE } else { THINK_OPEN };
            let remaining = &buf[i..];

            // Look for a complete marker.
            if let Some(idx) = remaining.find(marker) {
                let (before, _after) = remaining.split_at(idx);
                if self.inside {
                    reasoning.push_str(before);
                } else {
                    text.push_str(before);
                }
                self.inside = !self.inside;
                i += idx + marker.len();
                continue;
            }

            // No complete marker. Check whether the tail of `remaining`
            // could be the start of one — if so, buffer those bytes for
            // the next call. Anything before that goes out now.
            //
            // Markers are pure ASCII, so any matching tail is also pure
            // ASCII. We use `str::ends_with(&str)` (byte-based suffix
            // compare; doesn't slice into the string) to avoid panicking
            // on multi-byte codepoints near the end of `remaining`. Try
            // longest-prefix first (greedy: if `<thi` fits, hold it
            // rather than holding just `<`).
            let mut hold_len: Option<usize> = None;
            for back in (1..marker.len()).rev() {
                let candidate = &marker[..back];
                if remaining.ends_with(candidate) {
                    hold_len = Some(back);
                    break;
                }
            }

            if let Some(back) = hold_len {
                let split_at = remaining.len() - back;
                let (before, hold) = remaining.split_at(split_at);
                if self.inside {
                    reasoning.push_str(before);
                } else {
                    text.push_str(before);
                }
                self.pending = hold.to_string();
            } else if self.inside {
                reasoning.push_str(remaining);
            } else {
                text.push_str(remaining);
            }
            break;
        }

        (text, reasoning)
    }

    /// Flush any pending buffered bytes at end-of-stream. Called once
    /// after the last chunk arrives. Trailing partial-tag bytes are
    /// emitted to the text channel as a fallback (better to surface them
    /// than silently drop, in case the stream truly ended mid-tag).
    pub(crate) fn flush(&mut self) -> (String, String) {
        let pending = std::mem::take(&mut self.pending);
        if self.inside {
            (String::new(), pending)
        } else {
            (pending, String::new())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::providers::lookup_provider;

    #[test]
    fn maps_openai_finish_reasons() {
        assert_eq!(map_openai_finish_reason("stop"), FinishReason::Stop);
        assert_eq!(map_openai_finish_reason("length"), FinishReason::Length);
        assert_eq!(
            map_openai_finish_reason("tool_calls"),
            FinishReason::ToolUse
        );
        assert_eq!(
            map_openai_finish_reason("content_filter"),
            FinishReason::ContentFilter
        );
    }

    #[test]
    fn think_tags_stripped_via_feed_then_flush() {
        // The non-streaming InlineThinkTags path feeds the whole body then
        // flushes, splitting reasoning out of content (#5).
        let mut ts = ThinkTagState::new();
        let (mut text, mut reasoning) = ts.feed("<think>weighing</think>answer");
        let (t2, r2) = ts.flush();
        text.push_str(&t2);
        reasoning.push_str(&r2);
        assert_eq!(text, "answer");
        assert_eq!(reasoning, "weighing");
    }

    #[test]
    fn accumulate_tool_call_drops_implausible_index() {
        // H9: a stream-controlled huge index must not grow the Vec.
        let mut partials: Vec<PartialToolCall> = Vec::new();
        let delta: ToolCallDeltaWire =
            serde_json::from_value(serde_json::json!({"index": 1_000_000})).unwrap();
        accumulate_tool_call(&mut partials, delta);
        assert!(partials.is_empty(), "huge index must be dropped");

        // A normal index still accumulates.
        let ok: ToolCallDeltaWire =
            serde_json::from_value(serde_json::json!({"index": 0, "function": {"name": "x"}}))
                .unwrap();
        accumulate_tool_call(&mut partials, ok);
        assert_eq!(partials.len(), 1);
    }

    fn test_profile() -> &'static ProviderProfile {
        lookup_provider("openai").expect("openai is in the registry")
    }

    fn test_adapter() -> OpenAICompatAdapter {
        OpenAICompatAdapter::new(
            test_profile(),
            "https://api.openai.com/v1".to_string(),
            "test-key".to_string(),
            "gpt-5-mini".to_string(),
            HashMap::new(),
        )
        .expect("adapter constructs")
    }

    #[test]
    fn chat_completion_chunk_parses_usage_only_frame() {
        // #123: a final usage-only frame carries no `choices`; with the field
        // defaulted it must parse instead of failing "missing field choices".
        let chunk: ChatCompletionChunk = serde_json::from_str(
            r#"{"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#,
        )
        .expect("usage-only frame must parse");
        assert!(chunk.choices.is_empty());
        assert!(chunk.usage.is_some());
    }

    #[test]
    fn is_reasoning_model_matches_o_series_and_gpt5() {
        for m in [
            "o1",
            "o1-mini",
            "o3",
            "o3-mini",
            "o4-mini",
            "gpt-5",
            "gpt-5-mini",
        ] {
            assert!(is_reasoning_model(m), "{m} should be a reasoning model");
        }
        for m in ["gpt-4o", "gpt-4o-mini", "gpt-4-turbo", "chatgpt-4o-latest"] {
            assert!(
                !is_reasoning_model(m),
                "{m} should not be a reasoning model"
            );
        }
    }

    #[test]
    fn token_usage_from_wire_preserves_authoritative_total() {
        let usage = token_usage_from_wire(UsageWire {
            prompt_tokens: Some(100),
            completion_tokens: Some(25),
            total_tokens: Some(140),
            prompt_tokens_details: None,
            completion_tokens_details: None,
            input_tokens_details: None,
            output_tokens_details: None,
        });

        assert_eq!(usage.prompt_tokens, 100);
        assert_eq!(usage.completion_tokens, 25);
        assert_eq!(usage.total_tokens, 140);
    }

    #[test]
    fn token_usage_from_wire_falls_back_to_prompt_plus_completion() {
        let usage = token_usage_from_wire(UsageWire {
            prompt_tokens: Some(100),
            completion_tokens: Some(25),
            total_tokens: None,
            prompt_tokens_details: None,
            completion_tokens_details: None,
            input_tokens_details: None,
            output_tokens_details: None,
        });

        assert_eq!(usage.prompt_tokens, 100);
        assert_eq!(usage.completion_tokens, 25);
        assert_eq!(usage.total_tokens, 125);
    }

    #[test]
    fn token_usage_from_wire_preserves_cache_and_reasoning_details() {
        let usage = token_usage_from_wire(UsageWire {
            prompt_tokens: Some(100),
            completion_tokens: Some(25),
            total_tokens: Some(125),
            prompt_tokens_details: Some(PromptTokensDetailsWire {
                cached_tokens: Some(40),
            }),
            completion_tokens_details: Some(CompletionTokensDetailsWire {
                reasoning_tokens: Some(12),
            }),
            input_tokens_details: None,
            output_tokens_details: None,
        });

        assert_eq!(usage.cached_input_tokens, 40);
        assert_eq!(usage.reasoning_output_tokens, 12);
        assert_eq!(usage.total_tokens, 125);
    }

    #[test]
    fn cache_hit_does_not_double_count_input_total() {
        // #6: OpenAI nests cached tokens inside prompt_tokens; input_total must
        // be 100 (the real input), not 100 + 40.
        let usage = token_usage_from_wire(UsageWire {
            prompt_tokens: Some(100),
            completion_tokens: Some(25),
            total_tokens: Some(125),
            prompt_tokens_details: Some(PromptTokensDetailsWire {
                cached_tokens: Some(40),
            }),
            completion_tokens_details: None,
            input_tokens_details: None,
            output_tokens_details: None,
        });
        assert_eq!(usage.input_total_tokens(), 100);
        assert_eq!(usage.prompt_tokens, 60);
        assert_eq!(usage.cached_input_tokens, 40);
    }

    #[test]
    fn capabilities_reflect_profile() {
        let adapter = test_adapter();
        let caps = adapter.capabilities();
        assert!(caps.supports_tools);
        assert!(!caps.supports_vision);
        match &caps.supports_reasoning {
            ReasoningCapability::Levels(levels) => {
                assert!(levels.contains(&ReasoningLevel::Medium));
                assert!(levels.contains(&ReasoningLevel::Max));
            },
            other => panic!("expected Levels for openai, got {:?}", other),
        }
    }

    #[test]
    fn capabilities_unsupported_for_no_reasoning_provider() {
        let together = lookup_provider("together").unwrap();
        let adapter = OpenAICompatAdapter::new(
            together,
            together.base_url.to_string(),
            "k".to_string(),
            "deepseek-r1".to_string(),
            HashMap::new(),
        )
        .unwrap();
        assert_eq!(
            adapter.capabilities().supports_reasoning,
            ReasoningCapability::Unsupported
        );
    }

    #[test]
    fn name_returns_model_name() {
        let adapter = test_adapter();
        assert_eq!(adapter.name(), "gpt-5-mini");
    }

    #[test]
    fn build_request_body_includes_basic_fields() {
        let adapter = test_adapter();
        let messages = vec![ChatMessage::user("hello")];
        let config = ModelConfig::default();
        let body = adapter.build_request_body(&messages, &config, true);
        assert_eq!(body["model"], "gpt-5-mini");
        assert_eq!(body["stream"], true);
        assert!(body["messages"].is_array());
        // Default reasoning is Medium → Effort strategy emits the field.
        assert_eq!(body["reasoning_effort"], "medium");
    }

    #[test]
    fn build_request_body_includes_system_prompt() {
        let adapter = test_adapter();
        let messages = vec![ChatMessage::user("hi")];
        let config = ModelConfig {
            system_prompt: Some("You are a helpful assistant.".to_string()),
            ..Default::default()
        };
        let body = adapter.build_request_body(&messages, &config, false);
        let messages_arr = body["messages"].as_array().unwrap();
        assert_eq!(messages_arr[0]["role"], "system");
        assert_eq!(messages_arr[0]["content"], "You are a helpful assistant.");
    }

    /// Step 5h: OpenAI-compat doesn't expose per-block cache markers, so
    /// the dynamic MERMAID.md suffix is concatenated onto the static system
    /// message with a `---` separator. Single system message; both halves
    /// reach the model in one content payload.
    #[test]
    fn build_request_body_concats_dynamic_suffix_to_system_message() {
        let adapter = test_adapter();
        let messages = vec![ChatMessage::user("hi")];
        let config = ModelConfig {
            system_prompt: Some("You are Mermaid.".to_string()),
            dynamic_system_suffix: Some("Project rule: always snake_case.".to_string()),
            ..Default::default()
        };
        let body = adapter.build_request_body(&messages, &config, false);
        let messages_arr = body["messages"].as_array().unwrap();
        assert_eq!(messages_arr[0]["role"], "system");
        let content = messages_arr[0]["content"].as_str().unwrap();
        assert!(content.contains("You are Mermaid."));
        assert!(content.contains("Project rule: always snake_case."));
        assert!(content.contains("---"));
    }

    #[test]
    fn build_request_body_includes_tools_and_omits_temperature_for_reasoning() {
        // gpt-5-mini is a reasoning model: tools still pass through, but
        // `temperature` must be omitted — OpenAI 400s on it (#124).
        let adapter = test_adapter();
        let messages = vec![ChatMessage::user("hi")];
        // v7: tools come from config (populated by the provider
        // wrapper); adapter passes them through in OpenAI shape.
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
        let body = adapter.build_request_body(&messages, &config, true);
        assert!(body["tools"].is_array());
        assert_eq!(body["tools"].as_array().unwrap().len(), 5);
        assert!(
            body.get("temperature").is_none(),
            "a reasoning model must omit temperature, got {:?}",
            body.get("temperature")
        );
    }

    #[test]
    fn build_request_body_includes_temperature_for_non_reasoning_model() {
        // A non-reasoning model (gpt-4o) still receives `temperature` (#124).
        let adapter = OpenAICompatAdapter::new(
            test_profile(),
            "https://api.openai.com/v1".to_string(),
            "test-key".to_string(),
            "gpt-4o".to_string(),
            HashMap::new(),
        )
        .expect("adapter constructs");
        let config = ModelConfig::default();
        let body = adapter.build_request_body(&[ChatMessage::user("hi")], &config, false);
        assert_eq!(body["temperature"], config.temperature);
    }

    #[test]
    fn cerebras_uses_supported_token_budget_field() {
        let cerebras = lookup_provider("cerebras").unwrap();
        let adapter = OpenAICompatAdapter::new(
            cerebras,
            cerebras.base_url.to_string(),
            "k".to_string(),
            "gpt-oss-120b".to_string(),
            HashMap::new(),
        )
        .unwrap();
        let messages = vec![ChatMessage::user("hi")];
        let config = ModelConfig {
            max_tokens: 1234,
            ..Default::default()
        };
        let body = adapter.build_request_body(&messages, &config, true);
        assert_eq!(body["max_completion_tokens"], 1234);
        assert!(body.get("max_tokens").is_none());
    }

    #[test]
    fn cerebras_gpt_oss_disables_parallel_tool_calls() {
        let cerebras = lookup_provider("cerebras").unwrap();
        let adapter = OpenAICompatAdapter::new(
            cerebras,
            cerebras.base_url.to_string(),
            "k".to_string(),
            "gpt-oss-120b".to_string(),
            HashMap::new(),
        )
        .unwrap();
        let messages = vec![ChatMessage::user("hi")];
        let config = ModelConfig {
            tools: vec![serde_json::json!({
                "type": "function",
                "function": {
                    "name": "read_file",
                    "description": "read a file",
                    "parameters": {"type": "object"}
                }
            })],
            ..Default::default()
        };
        let body = adapter.build_request_body(&messages, &config, true);
        assert_eq!(body["parallel_tool_calls"], false);
    }

    #[test]
    fn build_request_body_omits_reasoning_for_none_strategy() {
        let together = lookup_provider("together").unwrap();
        let adapter = OpenAICompatAdapter::new(
            together,
            together.base_url.to_string(),
            "k".to_string(),
            "deepseek-r1".to_string(),
            HashMap::new(),
        )
        .unwrap();
        let messages = vec![ChatMessage::user("hi")];
        let config = ModelConfig::default();
        let body = adapter.build_request_body(&messages, &config, true);
        assert!(body.get("reasoning_effort").is_none());
        assert!(body.get("reasoning").is_none());
    }

    /// XHigh on an Effort-strategy provider round-trips intact as
    /// `reasoning_effort: "xhigh"`. OpenAI GPT-5.2+ honors it; other
    /// providers on this strategy will 400 (explicit failure is
    /// preferable to silent downgrade).
    #[test]
    fn build_request_body_emits_xhigh_for_xhigh_level() {
        let adapter = test_adapter();
        let messages = vec![ChatMessage::user("hi")];
        let config = ModelConfig {
            reasoning: ReasoningLevel::XHigh,
            ..Default::default()
        };
        let body = adapter.build_request_body(&messages, &config, true);
        assert_eq!(body["reasoning_effort"], "xhigh");
    }

    /// None on Effort emits the explicit `"none"` string (GPT-5.1+)
    /// rather than omitting the field — the user explicitly asked for
    /// no reasoning, and we propagate that intent.
    #[test]
    fn build_request_body_emits_none_for_none_level() {
        let adapter = test_adapter();
        let messages = vec![ChatMessage::user("hi")];
        let config = ModelConfig {
            reasoning: ReasoningLevel::None,
            ..Default::default()
        };
        let body = adapter.build_request_body(&messages, &config, true);
        assert_eq!(body["reasoning_effort"], "none");
    }

    /// `Minimal` is in the `Effort`-strategy supported set, so it
    /// round-trips intact (OpenAI GPT-5 honors `reasoning_effort:
    /// "minimal"`). This locks in the no-silent-drop guarantee for the
    /// only level that's restricted to a single provider.
    #[test]
    fn build_request_body_preserves_minimal_for_effort_strategy() {
        let adapter = test_adapter();
        let messages = vec![ChatMessage::user("hi")];
        let config = ModelConfig {
            reasoning: ReasoningLevel::Minimal,
            ..Default::default()
        };
        let body = adapter.build_request_body(&messages, &config, true);
        assert_eq!(body["reasoning_effort"], "minimal");
    }

    /// OpenRouter's normalized object has no `minimal` tier — `Minimal`
    /// requests must snap to the next-lowest supported level (`Low`)
    /// rather than silently sending `None` or 400ing. Verifies the
    /// `nearest_effort` wire-up works for the snap-down case.
    #[test]
    fn build_request_body_snaps_minimal_to_low_for_openrouter() {
        let openrouter = lookup_provider("openrouter").unwrap();
        let adapter = OpenAICompatAdapter::new(
            openrouter,
            openrouter.base_url.to_string(),
            "k".to_string(),
            "anthropic/claude-3.7-sonnet".to_string(),
            HashMap::new(),
        )
        .unwrap();
        let messages = vec![ChatMessage::user("hi")];
        let config = ModelConfig {
            reasoning: ReasoningLevel::Minimal,
            ..Default::default()
        };
        let body = adapter.build_request_body(&messages, &config, true);
        // Minimal isn't in OpenRouter's supported set; nearest_effort
        // returns None (highest at-or-below). When None lands in
        // OpenRouterShape.render, it emits {exclude: true}.
        assert_eq!(body["reasoning"], json!({"exclude": true}));
    }

    #[test]
    fn build_request_body_uses_openrouter_shape() {
        let openrouter = lookup_provider("openrouter").unwrap();
        let adapter = OpenAICompatAdapter::new(
            openrouter,
            openrouter.base_url.to_string(),
            "k".to_string(),
            "anthropic/claude-3.7-sonnet".to_string(),
            HashMap::new(),
        )
        .unwrap();
        let messages = vec![ChatMessage::user("hi")];
        let config = ModelConfig {
            reasoning: ReasoningLevel::High,
            ..Default::default()
        };
        let body = adapter.build_request_body(&messages, &config, true);
        assert_eq!(body["reasoning"], json!({"effort": "high"}));
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn tool_call_accumulator_assembles_fragmented_args() {
        // Simulate the standard 3-chunk OpenAI tool-call streaming
        // pattern: chunk 1 carries id+name, chunks 2/3 carry argument
        // string fragments.
        let mut partials: Vec<PartialToolCall> = Vec::new();

        accumulate_tool_call(
            &mut partials,
            ToolCallDeltaWire {
                index: 0,
                id: Some("call_abc".to_string()),
                type_: Some("function".to_string()),
                function: Some(FunctionDeltaWire {
                    name: Some("get_weather".to_string()),
                    arguments: Some(String::new()),
                }),
            },
        );
        accumulate_tool_call(
            &mut partials,
            ToolCallDeltaWire {
                index: 0,
                id: None,
                type_: None,
                function: Some(FunctionDeltaWire {
                    name: None,
                    arguments: Some("{\"loc".to_string()),
                }),
            },
        );
        accumulate_tool_call(
            &mut partials,
            ToolCallDeltaWire {
                index: 0,
                id: None,
                type_: None,
                function: Some(FunctionDeltaWire {
                    name: None,
                    arguments: Some("\":\"SF\"}".to_string()),
                }),
            },
        );

        let tc = partials
            .into_iter()
            .next()
            .unwrap()
            .into_tool_call()
            .unwrap();
        assert_eq!(tc.id.as_deref(), Some("call_abc"));
        assert_eq!(tc.function.name, "get_weather");
        assert_eq!(tc.function.arguments, json!({"loc": "SF"}));
    }

    #[test]
    fn tool_call_accumulator_handles_empty_args() {
        let mut partials: Vec<PartialToolCall> = Vec::new();
        accumulate_tool_call(
            &mut partials,
            ToolCallDeltaWire {
                index: 0,
                id: Some("call_x".to_string()),
                type_: None,
                function: Some(FunctionDeltaWire {
                    name: Some("list_windows".to_string()),
                    arguments: None,
                }),
            },
        );
        let tc = partials
            .into_iter()
            .next()
            .unwrap()
            .into_tool_call()
            .unwrap();
        assert_eq!(tc.function.arguments, json!({}));
    }

    #[test]
    fn tool_call_accumulator_handles_multiple_indices() {
        // Provider streams two parallel tool calls — index 0 and index 1
        // delta chunks interleaved.
        let mut partials: Vec<PartialToolCall> = Vec::new();
        accumulate_tool_call(
            &mut partials,
            ToolCallDeltaWire {
                index: 0,
                id: Some("call_a".to_string()),
                type_: None,
                function: Some(FunctionDeltaWire {
                    name: Some("fn_a".to_string()),
                    arguments: Some("{}".to_string()),
                }),
            },
        );
        accumulate_tool_call(
            &mut partials,
            ToolCallDeltaWire {
                index: 1,
                id: Some("call_b".to_string()),
                type_: None,
                function: Some(FunctionDeltaWire {
                    name: Some("fn_b".to_string()),
                    arguments: Some("{}".to_string()),
                }),
            },
        );

        let parsed: Vec<_> = partials
            .into_iter()
            .filter_map(|p| p.into_tool_call())
            .collect();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].function.name, "fn_a");
        assert_eq!(parsed[1].function.name, "fn_b");
    }

    // --- ThinkTagState (Wave 6) ---

    #[test]
    fn think_state_passes_plain_text_through() {
        let mut s = ThinkTagState::new();
        let (text, reasoning) = s.feed("hello world, no tags here");
        assert_eq!(text, "hello world, no tags here");
        assert!(reasoning.is_empty());
        let (tail_text, tail_reasoning) = s.flush();
        assert!(tail_text.is_empty());
        assert!(tail_reasoning.is_empty());
    }

    #[test]
    fn think_state_extracts_complete_tag_pair_in_one_chunk() {
        let mut s = ThinkTagState::new();
        let (text, reasoning) = s.feed("before<think>reasoning content</think>after");
        assert_eq!(text, "beforeafter");
        assert_eq!(reasoning, "reasoning content");
    }

    #[test]
    fn think_state_handles_tag_split_across_chunks() {
        let mut s = ThinkTagState::new();
        // Chunk 1 ends mid-opening-tag.
        let (text1, reasoning1) = s.feed("before<thi");
        assert_eq!(text1, "before");
        assert!(reasoning1.is_empty());
        // Chunk 2 completes the opening tag and includes the closing tag.
        let (text2, reasoning2) = s.feed("nk>X</think>after");
        assert_eq!(text2, "after");
        assert_eq!(reasoning2, "X");
    }

    #[test]
    fn think_state_handles_closing_tag_split() {
        let mut s = ThinkTagState::new();
        let (text1, reasoning1) = s.feed("<think>weighing options</thi");
        assert!(text1.is_empty());
        assert_eq!(reasoning1, "weighing options");
        let (text2, reasoning2) = s.feed("nk>final answer");
        assert_eq!(text2, "final answer");
        assert!(reasoning2.is_empty());
    }

    #[test]
    fn think_state_handles_multiple_tag_pairs() {
        let mut s = ThinkTagState::new();
        let (text, reasoning) = s.feed("a<think>r1</think>b<think>r2</think>c");
        assert_eq!(text, "abc");
        // Both reasoning runs come back concatenated since `feed`
        // returns one (text, reasoning) pair per call.
        assert_eq!(reasoning, "r1r2");
    }

    #[test]
    fn think_state_preserves_cjk_inside_tags() {
        let mut s = ThinkTagState::new();
        let (text, reasoning) = s.feed("英語<think>思考中</think>結果");
        assert_eq!(text, "英語結果");
        assert_eq!(reasoning, "思考中");
    }

    #[test]
    fn think_state_flush_emits_partial_tag_as_text() {
        let mut s = ThinkTagState::new();
        // Stream ends mid-opening-tag — partial bytes flush to text so
        // we don't silently drop user-visible content.
        let (text1, _) = s.feed("hello<thi");
        assert_eq!(text1, "hello");
        let (text_tail, reasoning_tail) = s.flush();
        assert_eq!(text_tail, "<thi");
        assert!(reasoning_tail.is_empty());
    }

    #[test]
    fn think_state_does_not_match_other_angle_brackets() {
        let mut s = ThinkTagState::new();
        let (text, reasoning) = s.feed("<other>tag-like</other> and <not a tag");
        // Output exactly the input — no `<think>` anywhere, so no split.
        // The tail `<not` would be buffered as a possible opening-tag
        // prefix, but since `<not` isn't a prefix of `<think>`, it
        // flushes through to text.
        assert_eq!(text, "<other>tag-like</other> and <not a tag");
        assert!(reasoning.is_empty());
    }

    #[test]
    fn truncation_marker_preserved_byte_for_byte() {
        // Sanity that this adapter's marker matches the agreed shape so
        // any consumer that greps for it (TUI's chat widget, log
        // scrapers) sees the same text.
        let mut buf = String::new();
        let mut t = false;
        push_capped(&mut buf, &"a".repeat(50), &mut t, 10);
        assert!(t);
        assert!(buf.ends_with(TRUNCATION_MARKER));
    }
}
