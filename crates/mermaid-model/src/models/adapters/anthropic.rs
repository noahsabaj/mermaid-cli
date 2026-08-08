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
//! enabled. Mermaid's `ChatMessage::provider_continuation` field (Step 3
//! Wave 1) holds it across turns. The signature is per-thinking-block
//! server state — drop it and the API returns 400 `invalid_request_error`
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

use super::ModelLimits;
use super::output_budget::{OutputBudgetInputs, OutputCapMode, resolve_output_budget};
use crate::models::types::{
    ChatMessage, FinishReason, MessageAudience, MessageRole, ModelResponse, ProviderContinuation,
    TokenUsage,
};
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

/// F56: whether an Anthropic stream ended abnormally — the connection closed
/// before ANY terminal frame was observed. A normal stream ends with a
/// `message_stop` event, preceded by a `message_delta` that carries the
/// terminal `stop_reason`. If NEITHER was seen the turn is truncated, not
/// complete; surfacing a clean `Ok` (with `stop_reason: None`) here would be
/// indistinguishable from a real completion, so the caller returns a stream
/// error instead. A `max_tokens` truncation is NOT abnormal — it arrives as a
/// real `stop_reason` (`Length`), so it's preserved and the runtime's
/// compact-and-continue path still fires.
fn stream_closed_abnormally(saw_message_stop: bool, stop_reason: Option<&FinishReason>) -> bool {
    !saw_message_stop && stop_reason.is_none()
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

/// Pick the thinking-config shape this Claude model accepts, from the
/// capability catalog. The 4.6+ line uses adaptive; the 4.5 family
/// (Sonnet 4.5 / Opus 4.5 / Haiku 4.5) uses legacy `budget_tokens`.
/// Defaults to `Legacy` for genuinely unknown models — if a future model
/// rejects legacy, the API's 400 names the fix, and the catalog should gain
/// a row when that model is added (the pre-catalog table predated Opus 4.8 /
/// Fable 5 and wrongly sent them legacy → 400).
fn thinking_format_for(model: &str) -> ThinkingFormat {
    match crate::models::catalog::lookup(model).thinking {
        crate::models::catalog::ThinkingShape::AnthropicAdaptive => ThinkingFormat::Adaptive,
        _ => ThinkingFormat::Legacy,
    }
}

/// Translate `ReasoningLevel` to a legacy `budget_tokens` value, clamped
/// so it never exceeds `max_tokens - 1024` (the API rejects budgets that
/// don't leave headroom for the actual output).
///
/// Budgets climb monotonically with rank so `XHigh` (between High and Max)
/// gets a between-the-two budget rather than collapsing onto either
/// neighbor. Legacy models don't expose `XHigh` on-paper, but the value
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
    // Anthropic requires `budget_tokens < max_tokens` with a 1024 floor; when
    // max_tokens can't fit a 1024 budget strictly below it, disable thinking
    // rather than emit `budget >= max_tokens` — a guaranteed 400 (#53).
    if max_tokens <= 1024 {
        return None;
    }
    let ceiling = max_tokens.saturating_sub(1024) as u32;
    Some(proposed.min(ceiling).max(1024))
}

/// Floor for AUTO `max_tokens` when live discovery didn't resolve an output
/// ceiling (models endpoint unreachable, or a gateway id it doesn't list).
/// Anthropic REQUIRES `max_tokens`, so some concrete value must go on the
/// wire; every current Claude model accepts 8192. Escape hatch for capped
/// gateway ids: an explicit user `max_tokens`.
const ANTHROPIC_FALLBACK_MAX_OUTPUT_TOKENS: usize = 8_192;

/// Rough prompt-token estimate (≈4 chars/token, mirroring the Ollama sizing
/// estimator); only used to bound `max_tokens` to the window room.
fn estimate_prompt_tokens(messages: &[ChatMessage], system: Option<&str>) -> usize {
    let chars =
        messages.iter().map(|m| m.content.len()).sum::<usize>() + system.map_or(0, str::len);
    chars / 4
}

/// Translate `ReasoningLevel` to Anthropic's `effort` string, gated by the
/// catalog's per-model [`EffortCeiling`]. The `effort` parameter shapes
/// overall token spend including text + tool calls (not just thinking); per
/// the official effort doc it's accepted by Mythos, Fable 5, Opus 4.5–4.8,
/// and Sonnet 4.6 — it ERRORS on Sonnet 4.5 / Haiku 4.5 and older, so those
/// get no effort field at all.
///
/// Snap semantics: `XHigh` sits BETWEEN `High` and `Max` — when a model's
/// ceiling doesn't cover the requested tier we snap DOWN to `"high"`, never
/// UP (the user picked something below max; delivering max would over-spend
/// their intent).
fn adaptive_effort_for(level: ReasoningLevel, model: &str) -> Option<&'static str> {
    use crate::models::catalog::EffortCeiling;
    let ceiling = crate::models::catalog::lookup(model).effort_ceiling;
    // Models that don't accept `effort` at all must get no effort field —
    // sending one 400s the request.
    if ceiling == EffortCeiling::None {
        return None;
    }
    match level {
        ReasoningLevel::None => None,
        ReasoningLevel::Minimal | ReasoningLevel::Low => Some("low"),
        ReasoningLevel::Medium => Some("medium"),
        ReasoningLevel::High => Some("high"),
        ReasoningLevel::XHigh => {
            if ceiling >= EffortCeiling::XHigh {
                Some("xhigh")
            } else {
                Some("high")
            }
        },
        ReasoningLevel::Max => {
            if ceiling >= EffortCeiling::Max {
                Some("max")
            } else {
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

/// Wrap harness steering in a `<system-reminder>` tag and emit it as a user
/// turn. `coalesce_consecutive_roles` folds it into the neighbouring user turn
/// when there is one, so this only has to get the tagging right.
///
/// The tag matters: untagged, the text reads as something the USER said, and
/// models answer it, thank the user for it, or treat it as a new instruction.
/// Standing alone after an assistant message is safe here because the
/// continuation design deliberately carries no assistant-prefill dependency.
fn push_system_reminder(out: &mut Vec<Value>, text: &str) {
    out.push(json!({
        "role": "user",
        "content": [{
            "type": "text",
            "text": format!("<system-reminder>\n{text}\n</system-reminder>"),
        }],
    }));
}

/// Normalize a message `content` field to a block array.
///
/// Anthropic accepts a bare string for a lone text block and `convert_messages`
/// emits that shape as a wire optimization, so both forms round-trip here. An
/// empty string yields no blocks: Anthropic rejects empty text blocks, so an
/// empty turn must contribute nothing to a merge rather than poison it.
fn content_blocks(content: &Value) -> Vec<Value> {
    match content {
        Value::Array(blocks) => blocks.clone(),
        Value::String(text) if !text.is_empty() => {
            vec![json!({"type": "text", "text": text})]
        },
        _ => Vec::new(),
    }
}

/// Collapse same-role neighbours so the payload always alternates.
///
/// Anthropic rejects a history whose roles do not alternate. The arms of
/// `convert_messages` each push naively, and several ordinary histories put
/// two same-role turns next to each other: a `Tool` batch followed by a typed
/// user message, a model-directed reminder sitting between two user turns
/// (a request that errored before any assistant turn committed, then a
/// retype), or two assistant turns from an interrupted continuation. Enforcing
/// the rule at the single exit point makes the invalid payload unrepresentable
/// however the history is shaped, instead of asking each arm to remember it.
///
/// Merged turns also get their block order normalized, because Anthropic has
/// placement rules of its own: `tool_result` blocks must lead a user turn and
/// `thinking` must lead an assistant turn. A merge that ignored those would
/// trade one 400 for another.
fn coalesce_consecutive_roles(msgs: Vec<Value>) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::with_capacity(msgs.len());
    for msg in msgs {
        let same_role = out.last().is_some_and(|prev| prev["role"] == msg["role"]);
        let Some(prev) = out.last_mut().filter(|_| same_role) else {
            out.push(msg);
            continue;
        };
        let incoming = content_blocks(&msg["content"]);
        if incoming.is_empty() {
            continue;
        }
        let mut blocks = content_blocks(&prev["content"]);
        blocks.extend(incoming);
        // Stable partition: the role's must-lead block kind first, the rest
        // in their original relative order behind it.
        let lead = if msg["role"] == "user" {
            "tool_result"
        } else {
            "thinking"
        };
        let (mut leading, rest): (Vec<Value>, Vec<Value>) =
            blocks.into_iter().partition(|b| b["type"] == lead);
        leading.extend(rest);
        prev["content"] = Value::Array(leading);
    }
    out
}

/// Translate Mermaid's `ChatMessage` history into Anthropic's
/// `(system, messages)` shape. The system prompt comes from
/// `ModelConfig::system_prompt`. `MessageRole::System` messages in the history
/// are TUI affordances and stay out of `messages` — EXCEPT model-directed ones
/// (`MessageAudience::ModelDirected`), which are harness steering the model
/// must see and ride a tagged user block instead of being dropped.
///
/// Consecutive `MessageRole::Tool` messages are merged into a single
/// user-role message with multiple `tool_result` content blocks because
/// tool results always render as user-role. Anthropic forbids consecutive
/// same-role messages in general, which `coalesce_consecutive_roles`
/// guarantees on the way out — no arm below has to maintain it.
///
/// Assistant messages with `thinking + provider_continuation` emit a
/// `thinking` content block paired with the `text/tool_use` blocks; the
/// signature round-trips so subsequent turns don't 400.
#[expect(
    clippy::too_many_lines,
    reason = "predates the lint; see .github/baselines/expect_budget.txt"
)]
fn convert_messages(messages: &[ChatMessage]) -> (Option<String>, Vec<Value>) {
    let mut system: Option<String> = None;
    let mut out: Vec<Value> = Vec::new();

    let mut i = 0;
    while i < messages.len() {
        let msg = &messages[i];
        match msg.role {
            MessageRole::System
                if msg.kind.audience() == MessageAudience::ModelDirected
                    && !msg.content.is_empty() =>
            {
                // Anthropic has no mid-conversation system role, but this
                // content exists to steer the model and must not be dropped
                // (it silently was — plan reminders, context markers,
                // auto-continue and stalled-turn nudges all vanished here).
                // Deliver it as a tagged block on the adjacent user turn:
                // that keeps the tail position the steering depends on and
                // leaves the cached system prefix untouched, while the tag
                // keeps it distinguishable from what the user actually typed.
                push_system_reminder(&mut out, &msg.content);
                i += 1;
            },
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
                if let (Some(thinking), Some(sig)) = (
                    &msg.thinking,
                    msg.provider_continuation
                        .as_ref()
                        .and_then(ProviderContinuation::anthropic_signature),
                ) && !thinking.is_empty()
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

    (system, coalesce_consecutive_roles(out))
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
    ///
    /// # Errors
    ///
    /// Only the HTTP client build can fail, as
    /// [`BackendError::ConnectionFailed`] — reqwest reads TLS roots and proxy
    /// settings from the environment there. Nothing here contacts the API, so
    /// an invalid key or unreachable `base_url` still constructs fine and
    /// fails on the first request.
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
            // Unknown until live discovery: the provider wrapper's
            // `resolve_context_window` fetches real per-model limits from
            // the Models API (cache-first). No static pins — they rot.
            max_context_tokens: None,
            max_output_tokens: None,
            emits_provider_continuation: false,
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

        // Anthropic REQUIRES `max_tokens`. AUTO (config.max_tokens == 0) sends
        // the model's live-discovered output ceiling (`resolved_max_output`,
        // from the Models API via the effect layer) or a conservative floor
        // when discovery didn't resolve; an explicit user cap is honored.
        // Both are bounded by the room the discovered window leaves after the
        // prompt, so `input + max_tokens` can't overrun the window (a 400).
        // `window: None` (unknown) applies no window clamp — the API's own
        // limit is the real gate.
        let max_tokens = resolve_output_budget(
            &OutputBudgetInputs {
                requested_cap: config.max_tokens,
                window: config.resolved_context_window,
                prompt_estimate: estimate_prompt_tokens(messages, system.as_deref()),
                provider_max_output: Some(
                    config
                        .resolved_max_output
                        .unwrap_or(ANTHROPIC_FALLBACK_MAX_OUTPUT_TOKENS),
                ),
                // Slop for the chars/4 estimate + structural JSON overhead.
                margin: 1_024,
                floor: 1,
            },
            OutputCapMode::Required,
        )
        .expect("Required mode always resolves a concrete max_tokens");

        let mut body = json!({
            "model": self.model_name,
            "messages": anthropic_messages,
            "max_tokens": max_tokens,
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
        // config doesn't get a 400. The 4.6+ adaptive line removed sampling
        // params entirely (Opus 4.7/4.8, Fable 5, Mythos) — sending any
        // temperature there is itself a 400, so only emit it where accepted.
        // The 4.6+ adaptive line removed sampling params — temperature 400s
        // on Opus 4.7/4.8, Fable 5, and Mythos (catalog column).
        if crate::models::catalog::lookup(&self.model_name).supports_temperature {
            let temp = config.temperature.clamp(0.0, 1.0);
            body["temperature"] = json!(temp);
        }

        // Tool registration is the single capability boundary. Translate every
        // registered tool; native fetch and SearXNG do not need an Ollama key.
        let registered: Vec<&Value> = config.tools.iter().collect();
        let mut anthropic_tools = to_anthropic_tools(&registered);
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

        // Native structured output: `output_config.format` (GA, no beta
        // header). Anthropic accepts a JSON-Schema subset (no recursion,
        // no numeric/string constraints, `additionalProperties: false`
        // required on objects) — arbitrary user schemas can 400, and older
        // models reject `format` entirely; the run falls back to the
        // prompt-driven turn and client-side validation remains the gate.
        if let Some(schema) = &config.output_schema {
            body["output_config"]["format"] = json!({
                "type": "json_schema",
                "schema": schema,
            });
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
                if let Some(budget) = legacy_budget_for(effective_reasoning, max_tokens) {
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
    /// via `crate::models::retry::retry_transient_http`.
    async fn send_chat(&self, body: &Value) -> Result<reqwest::Response> {
        let url = format!("{}/messages", self.base_url.trim_end_matches('/'));
        crate::models::retry::retry_transient_http(|| async {
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

    /// GET `{base_url}/models/{model}` — the Models API reports each model's
    /// real limits (`max_input_tokens` = context window, `max_tokens` =
    /// output ceiling). A 404 is a definitive "id not in the catalog"
    /// (gateway alias, fine-tune) → `Ok` all-`None` so callers can cache the
    /// absence; transport/auth/5xx failures are `Err` (never cached).
    ///
    /// # Errors
    ///
    /// Returns [`BackendError::ConnectionFailed`] when the request does not
    /// reach the API, the mapped HTTP error for any non-success status other
    /// than 404, and [`ModelError::ParseError`] when a success body is not the
    /// documented model-info shape. A 404 is deliberately not an error.
    pub async fn fetch_model_limits(&self) -> Result<ModelLimits> {
        let url = format!(
            "{}/models/{}",
            self.base_url.trim_end_matches('/'),
            self.model_name
        );
        let response = self
            .client
            .get(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .send()
            .await
            .map_err(|e| {
                ModelError::Backend(BackendError::ConnectionFailed {
                    backend: "anthropic".to_string(),
                    url: url.clone(),
                    reason: e.to_string(),
                })
            })?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(ModelLimits::default());
        }
        if !response.status().is_success() {
            return Err(http_error_from_response(response).await);
        }
        let info: AnthropicModelInfo =
            response.json().await.map_err(|e| ModelError::ParseError {
                message: format!("Failed to parse Anthropic model info: {e}"),
                raw: None,
            })?;
        Ok(info.into())
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
                message: format!("Failed to parse Anthropic response: {e}"),
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

        // Anthropic's `input_tokens` excludes both cache buckets, so the
        // components map 1:1. Thinking tokens ride inside `output_tokens`
        // (no separate reasoning count on this wire).
        let prompt_tokens = json.usage.input_tokens.unwrap_or(0);
        let completion_tokens = json.usage.output_tokens.unwrap_or(0);
        let cache_creation = json.usage.cache_creation_input_tokens.unwrap_or(0);
        let cache_read = json.usage.cache_read_input_tokens.unwrap_or(0);
        let usage = TokenUsage::provider(prompt_tokens, completion_tokens)
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
                debug: crate::models::error::ResponseDebugContext::default(),
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
            provider_continuation: signature
                .map(|signature| ProviderContinuation::Anthropic { signature }),
        })
    }

    /// Stream the response, emit typed events, return the final
    /// `ModelResponse`. Wave 3 implementation.
    #[expect(
        clippy::too_many_lines,
        reason = "predates the lint; see .github/baselines/expect_budget.txt"
    )]
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
        // F56: set when the terminal `message_stop` frame is observed, so an
        // abnormal close (connection dropped before any terminal frame) can be
        // told apart from a clean completion after the loop.
        let mut saw_message_stop = false;
        // Per-block-index accumulators. Anthropic emits content_block_*
        // events tagged with an `index` field; multiple blocks (text +
        // thinking + tool_use) interleave, so we track each by index.
        let mut blocks: HashMap<usize, BlockAccumulator> = HashMap::new();

        'stream: while let Some(chunk_result) = stream.next().await {
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
                            message: format!("Failed to parse Anthropic stream chunk: {e}"),
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
                                        // `ModelResponse.provider_continuation`
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
                        // Stream complete — record the terminal frame (F56)
                        // before breaking. Break the OUTER stream loop, not just
                        // this SSE-event `for` — otherwise the adapter keeps
                        // awaiting `stream.next()` until the connection actually
                        // closes, which can stall on a kept-alive/proxied body
                        // (#138). The `Done` event is emitted below after the loop.
                        saw_message_stop = true;
                        break 'stream;
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
                            debug: crate::models::error::ResponseDebugContext::default(),
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

        // F56: tell a genuinely abnormal close (the connection dropped before
        // ANY terminal frame) apart from a clean completion. If we saw neither
        // `message_stop` nor a `message_delta` `stop_reason`, the turn is
        // truncated — returning a clean `Ok` (with `stop_reason: None`) would be
        // indistinguishable from a real completion, and the open-block drain
        // below would even hand back partial content as if finished. Surface a
        // stream error instead. A `max_tokens` truncation set a real
        // `stop_reason`, so it does NOT trip this and is preserved.
        if stream_closed_abnormally(saw_message_stop, stop_reason.as_ref()) {
            return Err(ModelError::StreamError(
                "Anthropic stream closed before any terminal frame (message_stop / \
                 message_delta stop_reason); the connection was likely dropped \
                 mid-response"
                    .to_string(),
            ));
        }

        // The stream may end without a `message_stop` but WITH a `message_delta`
        // `stop_reason` (e.g. a proxy sends `Connection: close` after the final
        // delta). That's a complete turn missing only its framing event, so
        // finalize any blocks still open — a fully-streamed `tool_use` or text
        // block isn't silently dropped, and the agent doesn't "forget" the call.
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

        // F3: `Done` is emitted by the v0.7 wrapper from the returned
        // `ModelResponse` so the `provider_continuation` round-trips. If we
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
                debug: crate::models::error::ResponseDebugContext::default(),
            }));
        }

        Ok(ModelResponse {
            content: text_acc,
            usage: Some(
                TokenUsage::provider(prompt_tokens, completion_tokens)
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
            provider_continuation: signature_acc
                .map(|signature| ProviderContinuation::Anthropic { signature }),
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

    /// Anthropic DOES expose `GET /v1/models` these days — mermaid uses the
    /// per-model variant for limit discovery (`fetch_model_limits`) — but
    /// interactive model listing stays registry/config-driven, so this stub
    /// remains Unsupported rather than growing a third listing path.
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

/// `GET /v1/models/{id}` response — only the limit fields matter here.
/// `max_input_tokens` is the context window; `max_tokens` is the per-response
/// output ceiling. Both `#[serde(default)]` so an API that stops reporting
/// one degrades to `None` (unknown) instead of a parse error.
#[derive(Debug, Default, Deserialize)]
struct AnthropicModelInfo {
    #[serde(default)]
    max_input_tokens: Option<usize>,
    #[serde(default)]
    max_tokens: Option<usize>,
}

impl From<AnthropicModelInfo> for ModelLimits {
    fn from(info: AnthropicModelInfo) -> Self {
        Self {
            max_context_tokens: info.max_input_tokens,
            max_output_tokens: info.max_tokens,
        }
    }
}

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
        id: String,
        name: String,
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
/// `content_block` events for multiple blocks (text + thinking + `tool_use`),
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
    let debug = crate::models::error::ResponseDebugContext::from_headers(response.headers());
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
                    "{err_msg} (thinking-block round-trip failed; this is a Mermaid bug — \
                         please open an issue with the conversation that triggered it)"
                ),
                debug: debug.clone(),
            });
        }
        return ModelError::Backend(BackendError::ProviderError {
            provider: "anthropic".to_string(),
            code: Some(err_type.to_string()),
            message: err_msg.to_string(),
            debug: debug.clone(),
        });
    }
    ModelError::Backend(BackendError::HttpError {
        status,
        message: body,
        debug,
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
    fn model_info_parses_documented_limit_fields() {
        // Documented `GET /v1/models/{id}` shape (Models API): the limit
        // fields ride alongside identity fields we ignore.
        let body = r#"{
            "id": "claude-sonnet-4-6",
            "type": "model",
            "display_name": "Claude Sonnet 4.6",
            "created_at": "2026-02-01T00:00:00Z",
            "max_input_tokens": 1000000,
            "max_tokens": 128000
        }"#;
        let info: AnthropicModelInfo = serde_json::from_str(body).expect("parse");
        let limits: ModelLimits = info.into();
        assert_eq!(limits.max_context_tokens, Some(1_000_000));
        assert_eq!(limits.max_output_tokens, Some(128_000));
    }

    #[test]
    fn model_info_missing_limit_fields_degrade_to_none() {
        // An API that stops reporting limits must degrade to unknown, not a
        // parse error (which would be treated as a failed — uncached — fetch).
        let body = r#"{"id": "claude-sonnet-4-6", "type": "model"}"#;
        let info: AnthropicModelInfo = serde_json::from_str(body).expect("parse");
        let limits: ModelLimits = info.into();
        assert_eq!(limits.max_context_tokens, None);
        assert_eq!(limits.max_output_tokens, None);
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
    fn stream_closed_abnormally_distinguishes_drop_from_completion() {
        // F56: closed before ANY terminal frame (no message_stop, no
        // message_delta stop_reason) → abnormal, surfaced as a stream error.
        assert!(stream_closed_abnormally(false, None));
        // Clean completion: message_stop observed.
        assert!(!stream_closed_abnormally(true, Some(&FinishReason::Stop)));
        // Dropped after message_delta (stop_reason set) but before message_stop:
        // we have the real finish reason, so it's complete — NOT abnormal (the
        // open-block drain then recovers any fully-streamed tool_use/text).
        assert!(!stream_closed_abnormally(false, Some(&FinishReason::Stop)));
        // CRUCIAL: a max_tokens truncation arrives as a real stop_reason
        // (Length) — it must NOT be misclassified as an abnormal close.
        assert!(!stream_closed_abnormally(
            false,
            Some(&FinishReason::Length)
        ));
        // Defensive: a message_stop frame is terminal even with no stop_reason.
        assert!(!stream_closed_abnormally(true, None));
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

        let mut signed = ChatMessage::assistant("answer").with_provider_continuation(
            ProviderContinuation::Anthropic {
                signature: "sig123".to_string(),
            },
        );
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
        // Current models that the old table misclassified as Legacy → 400.
        assert_eq!(
            thinking_format_for("claude-opus-4-8"),
            ThinkingFormat::Adaptive
        );
        assert_eq!(
            thinking_format_for("claude-fable-5"),
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
        // #53: max_tokens at/below the 1024 floor can't fit a budget strictly
        // below it → None (a budget >= max_tokens is a guaranteed 400).
        assert_eq!(legacy_budget_for(ReasoningLevel::High, 1024), None);
        assert_eq!(legacy_budget_for(ReasoningLevel::Max, 512), None);
        // Just above the floor: a budget is returned and is strictly < max_tokens.
        let b = legacy_budget_for(ReasoningLevel::High, 2048).expect("fits");
        assert!(b < 2048, "budget {b} must be < max_tokens");
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

    /// Effort gating on the 4.5 family (RC-H). Sonnet 4.5 / Haiku 4.5 don't
    /// accept the `effort` parameter at all — it 400s — so they must get no
    /// effort field (`None`). Opus 4.5 accepts effort but not `max`, so `Max`
    /// and `XHigh` snap down to `high`.
    #[test]
    fn adaptive_effort_gates_max_on_4_5_family() {
        for m in ["claude-sonnet-4-5", "claude-haiku-4-5"] {
            assert_eq!(
                adaptive_effort_for(ReasoningLevel::Max, m),
                None,
                "model {m} does not support the effort parameter at all"
            );
            assert_eq!(
                adaptive_effort_for(ReasoningLevel::XHigh, m),
                None,
                "model {m} does not support the effort parameter at all"
            );
        }
        // Opus 4.5: supports effort but not `max` → snap down to `high`.
        assert_eq!(
            adaptive_effort_for(ReasoningLevel::Max, "claude-opus-4-5"),
            Some("high"),
            "Opus 4.5 should snap Max → high (no max effort support)"
        );
        assert_eq!(
            adaptive_effort_for(ReasoningLevel::XHigh, "claude-opus-4-5"),
            Some("high"),
            "Opus 4.5 should snap XHigh → high"
        );
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
        msg.provider_continuation = Some(ProviderContinuation::Anthropic {
            signature: "sig_xyz".to_string(),
        });
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
    fn auto_max_tokens_uses_live_discovered_ceiling() {
        // AUTO (max_tokens == 0) sends the live-discovered output ceiling —
        // a 1M-window / 128k-ceiling model gets the full 128k, and a tiny
        // prompt leaves the window's room above it.
        let adapter = test_adapter();
        let config = ModelConfig {
            max_tokens: 0,
            resolved_context_window: Some(1_000_000),
            resolved_max_output: Some(128_000),
            ..Default::default()
        };
        let body = adapter.build_request_body(&[ChatMessage::user("Hello")], &config);
        assert_eq!(body["max_tokens"], 128_000);
    }

    #[test]
    fn auto_max_tokens_floors_when_discovery_unresolved() {
        // Discovery failed (both resolved_* None): Anthropic still REQUIRES
        // max_tokens, so AUTO falls back to the conservative 8192 floor and
        // applies no window clamp.
        let adapter = test_adapter();
        let config = ModelConfig {
            max_tokens: 0,
            ..Default::default()
        };
        let body = adapter.build_request_body(&[ChatMessage::user("Hello")], &config);
        assert_eq!(body["max_tokens"], 8_192);
    }

    #[test]
    fn auto_max_tokens_clamps_to_window_room() {
        // A tight discovered window bounds AUTO below the output ceiling:
        // room = window − prompt_estimate − margin. Pin a tiny system prompt
        // so the estimate is deterministic: ("Hello" 5 + "sys" 3) / 4 = 2.
        let adapter = test_adapter();
        let config = ModelConfig {
            max_tokens: 0,
            system_prompt: Some("sys".to_string()),
            resolved_context_window: Some(16_384),
            resolved_max_output: Some(128_000),
            ..Default::default()
        };
        let body = adapter.build_request_body(&[ChatMessage::user("Hello")], &config);
        assert_eq!(body["max_tokens"], 16_384 - 2 - 1_024);
    }

    /// Harness steering (`RecoveryNudge`, `ContextMarker`) MUST reach the
    /// model. Anthropic has no mid-conversation system role, and this adapter
    /// used to drop such messages outright — silently deleting the plan-mode
    /// reminder, context markers, and the auto-continue and stalled-turn
    /// nudges on every `claude/*` model.
    #[test]
    fn model_directed_system_messages_reach_the_wire_as_tagged_user_blocks() {
        use crate::models::ChatMessageKind;
        let mut nudge = ChatMessage::system("Reminder: plan mode is active.");
        nudge.kind = ChatMessageKind::RecoveryNudge;
        let messages = vec![ChatMessage::user("ok"), nudge];

        let (_system, out) = convert_messages(&messages);
        assert_eq!(out.len(), 1, "merged into the adjacent user turn");
        assert_eq!(out[0]["role"], "user");
        let blocks = out[0]["content"].as_array().expect("content array");
        assert_eq!(blocks.len(), 2, "original text plus the reminder");
        assert_eq!(blocks[0]["text"], "ok");
        let tagged = blocks[1]["text"].as_str().unwrap();
        assert!(
            tagged.contains("<system-reminder>") && tagged.contains("plan mode is active"),
            "steering must be delivered and tagged: {tagged}",
        );
    }

    /// With no user turn to attach to, one is created rather than dropping the
    /// steering. (The output-cap continuation nudge lands right after an
    /// assistant partial; that design carries no prefill dependency.)
    #[test]
    fn model_directed_system_message_creates_a_user_turn_when_needed() {
        use crate::models::ChatMessageKind;
        let mut nudge = ChatMessage::system("Resume where you stopped.");
        nudge.kind = ChatMessageKind::ContextMarker;
        let messages = vec![ChatMessage::assistant("partial reply"), nudge];

        let (_system, out) = convert_messages(&messages);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["role"], "assistant");
        assert_eq!(out[1]["role"], "user", "alternation stays valid");
        assert!(
            out[1]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("Resume where you stopped"),
        );
    }

    // ── Role alternation ────────────────────────────────────────────────

    /// Anthropic rejects a history whose roles do not alternate, and several
    /// ordinary shapes put two same-role turns next to each other. Asserted
    /// as a property over the family rather than as one example: the ordering
    /// that motivated the fix (steering between two user turns — a request
    /// that errored before any assistant turn committed, then a retype) is
    /// only one member, and the next one added should be caught here.
    #[test]
    fn convert_messages_never_emits_consecutive_same_role_turns() {
        use crate::models::ChatMessageKind;
        let steering = || {
            let mut m = ChatMessage::system("Reminder: plan mode is active.");
            m.kind = ChatMessageKind::ContextMarker;
            m
        };
        let tool_call = || {
            let mut m = ChatMessage::assistant("");
            m.tool_calls = Some(vec![ToolCall {
                id: Some("c1".to_string()),
                function: FunctionCall {
                    name: "read_file".into(),
                    arguments: json!({"path": "a.txt"}),
                },
            }]);
            m
        };

        let shapes: Vec<(&str, Vec<ChatMessage>)> = vec![
            (
                "steering between two user turns",
                vec![
                    ChatMessage::user("first"),
                    steering(),
                    ChatMessage::user("second"),
                ],
            ),
            (
                "two user turns in a row",
                vec![ChatMessage::user("first"), ChatMessage::user("second")],
            ),
            (
                "user types while tool results are pending",
                vec![
                    ChatMessage::user("read it"),
                    tool_call(),
                    ChatMessage::tool("c1", "read_file", "contents"),
                    ChatMessage::user("actually, stop"),
                ],
            ),
            (
                "two assistant turns from an interrupted continuation",
                vec![
                    ChatMessage::user("go"),
                    ChatMessage::assistant("part one"),
                    ChatMessage::assistant("part two"),
                ],
            ),
            (
                "back-to-back steering",
                vec![ChatMessage::user("go"), steering(), steering()],
            ),
            (
                "steering with no user turn to attach to",
                vec![ChatMessage::assistant("partial"), steering()],
            ),
        ];

        for (name, messages) in shapes {
            let (_system, out) = convert_messages(&messages);
            assert!(!out.is_empty(), "{name}: the history must not vanish");
            for pair in out.windows(2) {
                assert_ne!(
                    pair[0]["role"], pair[1]["role"],
                    "{name}: emitted consecutive {} turns, which Anthropic rejects: {out:#?}",
                    pair[0]["role"],
                );
            }
        }
    }

    /// Coalescing must not lose content. An implementation that simply dropped
    /// the second of two same-role turns would satisfy the alternation
    /// property above, so the content has to be pinned separately.
    #[test]
    fn coalescing_two_user_turns_keeps_both_texts() {
        let messages = vec![ChatMessage::user("first"), ChatMessage::user("second")];
        let (_system, out) = convert_messages(&messages);
        assert_eq!(out.len(), 1);
        let blocks = out[0]["content"].as_array().expect("content array");
        assert_eq!(blocks.len(), 2, "both texts survive: {blocks:#?}");
        assert_eq!(blocks[0]["text"], "first");
        assert_eq!(blocks[1]["text"], "second");
    }

    /// `tool_result` blocks must LEAD the user turn they sit in. When a typed
    /// message merges into a pending tool batch, naive concatenation would put
    /// the text first — trading a role-alternation 400 for a placement one.
    #[test]
    fn merged_user_turn_keeps_tool_results_first() {
        let mut call = ChatMessage::assistant("");
        call.tool_calls = Some(vec![ToolCall {
            id: Some("c1".to_string()),
            function: FunctionCall {
                name: "read_file".into(),
                arguments: json!({"path": "a.txt"}),
            },
        }]);
        let messages = vec![
            ChatMessage::user("read it"),
            call,
            ChatMessage::tool("c1", "read_file", "contents"),
            ChatMessage::user("actually, stop"),
        ];
        let (_system, out) = convert_messages(&messages);
        let blocks = out[2]["content"].as_array().expect("content array");
        assert_eq!(out[2]["role"], "user");
        assert_eq!(blocks[0]["type"], "tool_result", "{blocks:#?}");
        assert_eq!(blocks[1]["type"], "text");
        assert_eq!(blocks[1]["text"], "actually, stop");
    }

    /// The assistant-side counterpart: `thinking` must lead its turn, so a
    /// merge that lands a thinking block behind a text block is a 400.
    #[test]
    fn merged_assistant_turn_keeps_thinking_first() {
        let mut second = ChatMessage::assistant("part two");
        second.thinking = Some("more reasoning".to_string());
        second.provider_continuation = Some(ProviderContinuation::Anthropic {
            signature: "sig_xyz".to_string(),
        });
        let messages = vec![
            ChatMessage::user("go"),
            ChatMessage::assistant("part one"),
            second,
        ];
        let (_system, out) = convert_messages(&messages);
        assert_eq!(out.len(), 2);
        let blocks = out[1]["content"].as_array().expect("content array");
        assert_eq!(blocks[0]["type"], "thinking", "{blocks:#?}");
        assert_eq!(blocks[1]["text"], "part one");
        assert_eq!(blocks[2]["text"], "part two");
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

    /// Native structured output rides in `output_config.format` and must
    /// merge with (not clobber) `output_config.effort` when both are set.
    #[test]
    fn build_request_body_maps_output_schema_to_output_config_format() {
        let adapter = test_adapter();
        let messages = vec![ChatMessage::user("format it")];
        let config = ModelConfig {
            reasoning: ReasoningLevel::High,
            output_schema: Some(serde_json::json!({
                "type": "object",
                "properties": {"answer": {"type": "integer"}}
            })),
            ..Default::default()
        };
        let body = adapter.build_request_body(&messages, &config);
        assert_eq!(body["output_config"]["format"]["type"], "json_schema");
        assert_eq!(body["output_config"]["format"]["schema"]["type"], "object");
        // Effort coexists in the same object.
        assert_eq!(body["output_config"]["effort"], "high");
        // Absent -> no format key at all.
        let body = adapter.build_request_body(&messages, &ModelConfig::default());
        assert!(body["output_config"].get("format").is_none());
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

    /// Sonnet 4.5 uses legacy `budget_tokens` thinking AND must NOT receive an
    /// `effort` field — the effort parameter 400s on Sonnet 4.5 / Haiku 4.5
    /// (RC-H: the old code sent effort to every model, including these). A
    /// temperature is still accepted here.
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
        // Effort is NOT supported on Sonnet 4.5 — emitting it would 400.
        assert!(
            body.get("output_config").is_none(),
            "Sonnet 4.5 must not get an effort field"
        );
        // Sampling params are still accepted on the 4.5 family.
        assert!(body.get("temperature").is_some());
    }

    /// RC-H: Opus 4.8 / Fable 5 are on the 4.6+ adaptive line — adaptive
    /// thinking, effort in `output_config`, and NO temperature (it 400s there).
    #[test]
    fn build_request_body_adaptive_no_temperature_for_opus_4_8() {
        let adapter = AnthropicAdapter::new(
            "k".to_string(),
            "claude-opus-4-8".to_string(),
            "https://api.anthropic.com/v1".to_string(),
        )
        .unwrap();
        let messages = vec![ChatMessage::user("Hi")];
        let config = ModelConfig {
            reasoning: ReasoningLevel::High,
            ..Default::default()
        };
        let body = adapter.build_request_body(&messages, &config);
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert!(
            body["thinking"].get("budget_tokens").is_none(),
            "Opus 4.8 rejects legacy budget_tokens"
        );
        assert_eq!(body["output_config"]["effort"], "high");
        assert!(
            body.get("temperature").is_none(),
            "Opus 4.8 rejects a top-level temperature"
        );
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

    /// Opus 4.7 + `XHigh` maps to `xhigh` — the highest tier, available
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

    /// Opus 4.5 accepts `effort` but not `max`, so the adapter snaps Max to
    /// `high` to avoid a 400. (Sonnet 4.5 / Haiku 4.5 get no effort field at
    /// all — covered by `build_request_body_uses_legacy_for_sonnet_4_5`.)
    #[test]
    fn build_request_body_snaps_max_to_high_on_opus_4_5() {
        let adapter = AnthropicAdapter::new(
            "k".to_string(),
            "claude-opus-4-5".to_string(),
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
            "Opus 4.5 should snap Max → high (no max effort support)"
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

    /// Step 5c: when the user enables `hide_reasoning_trace`, send
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

    #[test]
    fn build_request_body_preserves_registry_selected_web_tools() {
        let adapter = test_adapter();
        let config = ModelConfig {
            tools: ["web_fetch", "web_search"]
                .into_iter()
                .map(|name| {
                    serde_json::json!({
                        "type": "function",
                        "function": {
                            "name": name,
                            "description": "registered web tool",
                            "parameters": {"type": "object"}
                        }
                    })
                })
                .collect(),
            ..Default::default()
        };

        let body = adapter.build_request_body(&[ChatMessage::user("hi")], &config);
        let names: Vec<&str> = body["tools"]
            .as_array()
            .expect("tools array")
            .iter()
            .filter_map(|tool| tool.get("name").and_then(Value::as_str))
            .collect();
        assert_eq!(names, ["web_fetch", "web_search"]);
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
                "non-last tool should not carry cache_control: {tool:?}"
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
            other => panic!("expected Levels, got {other:?}"),
        }
    }

    #[test]
    fn name_returns_model_id() {
        let adapter = test_adapter();
        assert_eq!(adapter.name(), "claude-sonnet-4-6");
    }
}
