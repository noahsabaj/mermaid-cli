//! Ollama model adapter
//!
//! Provides unified interface to Ollama (both local and cloud) with connection pooling,
//! health monitoring, and zero-unwrap error handling.

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

use super::accumulator::{CappedText, error_body};
use crate::models::ModelCapabilities;
use crate::models::adapters::driver::{
    Flow, Framing, StreamProtocol, drive_stream, plain_http_error,
};
use crate::models::adapters::ollama_sizing::ModelDims;
use crate::models::config::{BackendConfig, ModelConfig};
use crate::models::error::{BackendError, ModelError, Result};
use crate::models::reasoning::{ReasoningChunk, ReasoningLevel};
use crate::models::stream::{StatusNotify, StreamEvent, StreamSink};
use crate::models::traits::Model;
use crate::models::types::{ChatMessage, FinishReason, MessageRole, ModelResponse, TokenUsage};

/// Mutable accumulators for stream processing, grouped to reduce parameter count.
struct StreamAccumulator {
    content: CappedText,
    thinking: CappedText,
    tool_calls: Vec<crate::models::ToolCall>,
    prompt_tokens: usize,
    completion_tokens: usize,
    /// Set once the terminal `done` chunk reports real eval counts. A stream cut
    /// before `done` (or a `done` without counts) leaves this `false`, so
    /// `usage()` returns `None` rather than a zero `TokenUsage` that would reset
    /// the reducer's context gauge (F54, mirrors gemini's `saw_usage` / #125).
    saw_usage: bool,
    /// Ollama's `done_reason` from the terminal chunk, mapped to a
    /// `FinishReason` for the final `ModelResponse` instead of `None` (#13).
    done_reason: Option<String>,
    /// True once Ollama's terminal `done` chunk has been observed (F56). The
    /// `done` chunk is the authoritative terminal frame; a stream that ends
    /// without it was dropped mid-response. Tracked SEPARATELY from `done_reason`
    /// on purpose: the context-full truncation arrives as a real `done` chunk
    /// (with `done_reason: "length"`), so keying the abnormal-close check off
    /// `saw_done` keeps that legitimate `Ok + FinishReason::Length` truncation
    /// from being misclassified as a stream error.
    saw_done: bool,
}

impl StreamAccumulator {
    /// Final token usage, or `None` when the stream never reported eval counts
    /// (e.g. it was cut before Ollama's terminal `done` chunk). Returning `None`
    /// rather than a zero `TokenUsage` keeps the reducer's context gauge from
    /// being reset to zero (F54, mirrors gemini's `saw_usage` guard / #125).
    fn usage(&self) -> Option<TokenUsage> {
        self.saw_usage
            .then(|| TokenUsage::provider(self.prompt_tokens, self.completion_tokens))
    }

    /// F56: whether the stream closed abnormally — it ended before Ollama's
    /// terminal `done` chunk was ever observed (a connection dropped
    /// mid-response). Returning a clean `Ok` here would be indistinguishable
    /// from a real completion. Keyed off `saw_done` (the terminal frame), NOT
    /// `done_reason`, so a context-full truncation — a real `done` chunk whose
    /// `done_reason` is `"length"`, surfaced as `Ok + FinishReason::Length` for
    /// the runtime's compact-and-continue — is NOT misclassified as an error.
    fn closed_abnormally(&self) -> bool {
        !self.saw_done
    }
}

/// Brings a dead *local* model server back up.
///
/// Injected rather than called directly. Spawning a process is not a wire
/// adapter's job, and while the adapter reached into `ollama::ensure_running`
/// itself, the recovery was invisible from the call site — which is how a
/// connection-refused retry sat inside a read-only listing path unnoticed (see
/// [`crate::models::retry::retry_transient_http_no_connect_retry`]).
///
/// The provider layer supplies an implementation when the user's config allows
/// autostart. Enumeration verbs simply pass `None`, and that absence *is* the
/// read-only guarantee: observing state cannot start a server it has no way to
/// start.
#[async_trait]
pub trait LocalServerRecovery: Send + Sync {
    /// `Ok(())` once the server is up. `Err(Some(hint))` carries an actionable
    /// next step to append to the connection error; `Err(None)` means there is
    /// nothing useful to add.
    async fn ensure_running(
        &self,
        base_url: &str,
        notify: Option<&(dyn for<'a> Fn(&'a str) + Sync)>,
    ) -> std::result::Result<(), Option<String>>;
}

/// Ollama model adapter
pub struct OllamaAdapter {
    client: Client,
    base_url: String,
    model_name: String,
    capabilities: ModelCapabilities,
    /// Whether the model advertises the `thinking` capability via `/api/show`.
    /// Probed lazily on the first chat and cached, so `new` stays network-free.
    /// `None` until resolved; recent Ollama 400s a `think` field sent to a
    /// non-thinking model, so the send is gated on this (#122).
    thinking_cap: tokio::sync::OnceCell<bool>,
    /// Whether the model advertises the `vision` capability via `/api/show`.
    /// Probed lazily and cached like `thinking_cap`. Drives the no-vision-model
    /// warning (sending an image to a model that can't see it is silently
    /// ignored by Ollama); never gates the send.
    vision_cap: tokio::sync::OnceCell<bool>,
    /// How to revive a dead local server when a request is refused. `Some`
    /// only when the caller opted in — `BackendConfig::ollama_autostart` is
    /// what the provider layer consults before attaching one. The user should
    /// never have to leave mermaid to run `ollama serve`, but the adapter no
    /// longer decides that for itself.
    recovery: Option<Arc<dyn LocalServerRecovery>>,
    /// Fallback surface for the autostart notice on paths that carry no
    /// stream sink (`list_models` — the startup preflight and the CLI list
    /// verbs). Console constructors attach an stderr printer via
    /// [`OllamaAdapter::with_status_notify`]; the chat path's sink takes
    /// precedence. `None` (the default) keeps recovery silent, which is the
    /// safe choice anywhere a TUI might own the screen.
    status_notify: Option<StatusNotify>,
}

/// True if this model takes the gpt-oss `think: "low"|"medium"|"high"` string
/// enum instead of Ollama's usual `think: bool` — per the capability catalog
/// (case-insensitive prefix, so tagged variants like `gpt-oss:20b` and
/// `gpt-oss:120b-cloud` all route correctly).
fn uses_effort_string_think(model_name: &str) -> bool {
    matches!(
        crate::models::catalog::lookup(model_name).thinking,
        crate::models::catalog::ThinkingShape::OllamaEffortString
    )
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
/// `supports_thinking` is the model's advertised `thinking` capability (probed
/// lazily); `None` from the call site means "send as before" (unknown). Returns
/// `None` when no `think` field should be sent at all — a non-thinking model
/// 400s on a stray `think` (#122).
fn think_for_ollama(
    model_name: &str,
    level: ReasoningLevel,
    supports_thinking: bool,
) -> Option<serde_json::Value> {
    if uses_effort_string_think(model_name) {
        let effort = match level {
            // gpt-oss can't truly disable thinking. `None` collapses to
            // `"low"` (the closest-to-off tier) rather than silently
            // upgrading the user's explicit choice to `"medium"`.
            ReasoningLevel::None | ReasoningLevel::Minimal | ReasoningLevel::Low => "low",
            ReasoningLevel::Medium => "medium",
            ReasoningLevel::High | ReasoningLevel::Max | ReasoningLevel::XHigh => "high",
        };
        return Some(serde_json::Value::String(effort.to_string()));
    }
    if !supports_thinking {
        // Model advertised no `thinking` capability — omit the field entirely.
        return None;
    }
    Some(serde_json::Value::Bool(level != ReasoningLevel::None))
}

impl OllamaAdapter {
    /// Create a new Ollama adapter for a specific model
    ///
    /// # Errors
    ///
    /// Only the HTTP client build can fail, as
    /// [`BackendError::ConnectionFailed`]. Despite being `async`, this awaits
    /// nothing on the wire: the server is never contacted, and the model's
    /// `thinking`/`vision` capabilities are probed lazily on first use. A
    /// stopped Ollama still constructs fine.
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
        let capabilities = if uses_effort_string_think(model_name) {
            ModelCapabilities::advertised(
                false,
                crate::models::ReasoningCapability::Levels(vec![
                    ReasoningLevel::None,
                    ReasoningLevel::Low,
                    ReasoningLevel::Medium,
                    ReasoningLevel::High,
                ]),
            )
        } else {
            ModelCapabilities::ollama_default()
        };

        Ok(Self {
            client,
            base_url,
            model_name: model_name.to_string(),
            capabilities,
            thinking_cap: tokio::sync::OnceCell::new(),
            vision_cap: tokio::sync::OnceCell::new(),
            recovery: None,
            status_notify: None,
        })
    }

    /// Attach the hook that revives a dead local server. Callers pass one only
    /// when `BackendConfig::ollama_autostart` is set; leaving it off is what
    /// makes a path strictly read-only.
    pub fn with_recovery(mut self, recovery: Arc<dyn LocalServerRecovery>) -> Self {
        self.recovery = Some(recovery);
        self
    }

    /// Attach a surface for the autostart notice on sink-less paths
    /// (`list_models`). For console-owned contexts only — the startup
    /// preflight and the CLI list verbs print it to stderr; never attach
    /// anything that writes to a terminal a TUI might own.
    pub fn with_status_notify(mut self, notify: StatusNotify) -> Self {
        self.status_notify = Some(notify);
        self
    }

    /// Whether this model supports `think`. Probes `/api/show` `capabilities`
    /// once and caches the answer. Recent Ollama returns a non-empty
    /// `capabilities` array; if it lacks `thinking` we must NOT send `think`
    /// (it 400s). A probe failure or an empty/absent array (older Ollama, which
    /// tolerates a stray `think`) is treated as "unknown" → keep the prior
    /// send-`think` behavior and don't cache, so a transient blip retries.
    async fn thinking_supported(&self) -> bool {
        *self
            .thinking_cap
            .get_or_try_init(|| async {
                match self.probe_capabilities().await {
                    Some(caps) if !caps.is_empty() => Ok(caps.iter().any(|c| c == "thinking")),
                    _ => Err(()),
                }
            })
            .await
            .unwrap_or(&true)
    }

    /// Whether this model advertises the `vision` capability via `/api/show`,
    /// probed lazily and cached. Mirrors `thinking_supported`: a failed/empty
    /// probe is treated as "unknown" and NOT cached (so a transient blip
    /// retries), defaulting to `true` so we never falsely warn "no vision" on an
    /// older Ollama that omits the capabilities array. Consumed by the provider
    /// to drive the no-vision-model warning; never gates the send.
    pub async fn vision_supported(&self) -> bool {
        *self
            .vision_cap
            .get_or_try_init(|| async {
                match self.probe_capabilities().await {
                    Some(caps) if !caps.is_empty() => Ok(caps.iter().any(|c| c == "vision")),
                    _ => Err(()),
                }
            })
            .await
            .unwrap_or(&true)
    }

    /// Best-effort `/api/show` probe for the model's advertised `capabilities`
    /// array (e.g. `["completion", "tools", "thinking"]`). `None` on any error.
    async fn probe_capabilities(&self) -> Option<Vec<String>> {
        let url = format!("{}/api/show", self.base_url);
        // Each failure is logged before the caller assumes "supported":
        // "probe failed" and "probe said yes" used to be indistinguishable,
        // and the wrong guess is a 400 from a model that lacks `thinking`.
        let resp = match self
            .client
            .post(&url)
            .json(&json!({ "model": self.model_name }))
            .timeout(std::time::Duration::from_secs(
                crate::constants::OLLAMA_PROBE_TIMEOUT_SECS,
            ))
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(error) => {
                tracing::warn!(model = %self.model_name, %error, "ollama capability probe failed; assuming support");
                return None;
            },
        };
        if !resp.status().is_success() {
            tracing::warn!(model = %self.model_name, status = resp.status().as_u16(), "ollama capability probe rejected; assuming support");
            return None;
        }
        match resp.json::<OllamaShowResponse>().await {
            Ok(show) => Some(show.capabilities),
            Err(error) => {
                tracing::warn!(model = %self.model_name, %error, "ollama capability probe returned an unreadable body; assuming support");
                None
            },
        }
    }

    /// Probe `/api/show` for the model's real context window + architecture
    /// dimensions, and `/api/tags` for its weight size. These drive auto-sizing
    /// of `num_ctx`/`num_predict`. Best-effort: any error (server down, parse
    /// failure, timeout) returns `None` and the caller falls back to Ollama's
    /// defaults / the conservative cap. A short per-request timeout keeps a
    /// slow/hung server from stalling the turn.
    pub async fn show_model_info(&self) -> Option<OllamaModelInfo> {
        let url = format!("{}/api/show", self.base_url);
        let resp = self
            .client
            .post(&url)
            .json(&json!({ "model": self.model_name }))
            .timeout(std::time::Duration::from_secs(
                crate::constants::OLLAMA_PROBE_TIMEOUT_SECS,
            ))
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let show: OllamaShowResponse = resp.json().await.ok()?;
        let context_length = context_length_from_model_info(&show.model_info);
        let dims = dims_from_model_info(&show.model_info);
        let weight_bytes = self.model_size_bytes().await;

        // Nothing useful → signal absence so the caller retries cheaply next turn
        // rather than caching a useless result.
        if context_length.is_none() && dims.is_none() && weight_bytes.is_none() {
            return None;
        }
        Some(OllamaModelInfo {
            context_length,
            dims,
            weight_bytes,
        })
    }

    /// This model's on-disk byte size from `/api/tags`, or `None`. Used as the
    /// VRAM weight footprint subtracted from the auto-sizing budget.
    async fn model_size_bytes(&self) -> Option<u64> {
        let url = format!("{}/api/tags", self.base_url);
        let resp = self
            .client
            .get(&url)
            .timeout(std::time::Duration::from_secs(
                crate::constants::OLLAMA_PROBE_TIMEOUT_SECS,
            ))
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let tags: OllamaTagsResponse = resp.json().await.ok()?;
        tags.models
            .into_iter()
            .find(|m| m.name == self.model_name)
            .and_then(|m| m.size)
    }

    /// This model's current memory placement from `/api/ps` as
    /// `(size_vram, total)` bytes, or `None` if it isn't loaded / Ollama is
    /// unreachable / either figure is missing. `size_vram < total` means the
    /// model is split between GPU and CPU/RAM (partial offload → slow). Probed
    /// after a turn, once the model is resident.
    pub async fn model_placement(&self) -> Option<(u64, u64)> {
        let url = format!("{}/api/ps", self.base_url);
        let resp = self
            .client
            .get(&url)
            .timeout(std::time::Duration::from_secs(
                crate::constants::OLLAMA_PROBE_TIMEOUT_SECS,
            ))
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let ps: OllamaPsResponse = resp.json().await.ok()?;
        ps.models
            .into_iter()
            .find(|m| m.name == self.model_name)
            .and_then(|m| Some((m.size_vram?, m.size?)))
    }

    /// Handle a streaming response, emitting typed `StreamEvent`s onto the
    /// turn's sink.
    async fn handle_stream(
        &self,
        response: reqwest::Response,
        sink: Option<&StreamSink>,
    ) -> Result<ModelResponse> {
        if !response.status().is_success() {
            return Err(plain_http_error(response).await);
        }
        drive_stream(
            response.bytes_stream(),
            OllamaStream::new(self.model_name.clone()),
            sink,
        )
        .await
    }

    /// Process a single parsed stream chunk, updating accumulators and
    /// producing typed events.
    ///
    /// Event ordering within a chunk: reasoning (if any) → tool calls (if
    /// any) → text (if any). Events are pushed onto `out` rather than sent,
    /// so this stays synchronous and the caller owns every `await` — which
    /// is what makes the emission order the production order. The `Done`
    /// event comes from the provider wrapper, never from here.
    ///
    /// Once a buffer's `MAX_RESPONSE_CHARS` cap trips (`CappedText`, one flag
    /// per buffer), further chunks for it are silently dropped — both from
    /// the accumulator AND from typed-event emission. Tool calls and token
    /// usage are still recorded because those are bounded.
    fn process_stream_chunk(
        json_chunk: &OllamaStreamChunk,
        out: &mut Vec<StreamEvent>,
        acc: &mut StreamAccumulator,
    ) {
        // Reasoning / thinking content: emitted to the sink and recorded
        // into `acc.thinking` so `ModelResponse.thinking` stays populated
        // for callers that read the final response.
        if let Some(ref thinking_chunk) = json_chunk.message.thinking
            && acc.thinking.accepting()
            && !thinking_chunk.is_empty()
        {
            out.push(StreamEvent::Reasoning(ReasoningChunk {
                text: thinking_chunk.clone(),
                signature: None,
            }));
            acc.thinking.push(thinking_chunk);
        }

        // Tool calls — bounded, no cap needed. Emitted as typed events
        // immediately so streaming consumers can react before completion.
        if let Some(ref tool_calls) = json_chunk.message.tool_calls {
            acc.tool_calls.extend(tool_calls.clone());
            for tc in tool_calls {
                out.push(StreamEvent::ToolCall(tc.clone()));
            }
        }

        // Regular text content.
        if !json_chunk.message.content.is_empty() && acc.content.accepting() {
            out.push(StreamEvent::Text(json_chunk.message.content.clone()));
            acc.content.push(&json_chunk.message.content);
        }

        // Capture token usage + stop reason from the `done` chunk. `saw_usage`
        // is set only when a real eval count arrives, so a stream cut before
        // `done` reports `None` usage instead of zero (F54).
        if json_chunk.done {
            // F56: the terminal frame arrived — the stream completed normally
            // (even when `done_reason`/eval counts are absent).
            acc.saw_done = true;
            if let Some(count) = json_chunk.prompt_eval_count {
                acc.prompt_tokens = count;
                acc.saw_usage = true;
            }
            if let Some(count) = json_chunk.eval_count {
                acc.completion_tokens = count;
                acc.saw_usage = true;
            }
            if json_chunk.done_reason.is_some() {
                acc.done_reason = json_chunk.done_reason.clone();
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
        supports_thinking: bool,
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

        // Tools come from `config.tools` (populated by the provider wrapper
        // from `ChatRequest.tools`). The registry only registers a web tool
        // when its backend is usable — native `web_fetch` needs no key, and the
        // Ollama-backed web tools are gated on `OLLAMA_API_KEY` at registration
        // — so whatever reaches here is advertisable as-is.
        let tools: Vec<&serde_json::Value> = config.tools.iter().collect();

        let mut request_body = json!({
            "model": self.model_name,
            "messages": json_messages,
            "stream": stream,
            "tools": &tools,
        });

        // `--output-schema` formatting turn: Ollama's structured output.
        if let Some(schema) = &config.output_schema {
            request_body["format"] = schema.clone();
        }

        // `think` parameter: most Ollama models accept `think: bool`, gpt-oss
        // requires a string enum, and a model that doesn't advertise `thinking`
        // must not receive the field at all (it 400s). `think_for_ollama`
        // returns `None` in that last case so the key is omitted (#122).
        if let Some(think) = think_for_ollama(&self.model_name, config.reasoning, supports_thinking)
        {
            request_body["think"] = think;
        }
        tracing::debug!(
            "think reasoning={:?} supports_thinking={} shape={}",
            config.reasoning,
            supports_thinking,
            if uses_effort_string_think(&self.model_name) {
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
        // Clamp to the conventional 0..=2 range (matches the other adapters).
        options["temperature"] = json!(config.temperature.clamp(0.0, 2.0));
        if let Some(num_ctx) = ollama_opts.num_ctx {
            options["num_ctx"] = json!(num_ctx);
        }
        // Output cap. Without this Ollama generates unbounded and only stops when
        // the (often tiny default) num_ctx fills — the truncation bug. Derived
        // from max_tokens + reasoning headroom in `build_model_config`.
        if let Some(num_predict) = ollama_opts.num_predict {
            options["num_predict"] = json!(num_predict);
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
        tracing::debug!(
            "Ollama sizing: num_ctx={:?} num_predict={:?}",
            ollama_opts.num_ctx,
            ollama_opts.num_predict
        );
        request_body["options"] = options;

        request_body
    }

    /// POST /api/chat with the given body and return the raw response.
    /// Transparently retries on 5xx, 429, or reqwest connect failures
    /// via `crate::models::retry::retry_transient_http`. Mid-stream failures
    /// (body consumption) are NOT retried — partial content has already
    /// reached the caller at that point.
    async fn send_chat(
        &self,
        body: &serde_json::Value,
        sink: Option<&StreamSink>,
    ) -> Result<reqwest::Response> {
        let url = format!("{}/api/chat", self.base_url);
        self.with_local_recovery(sink, || async {
            self.client.post(&url).json(body).send().await.map_err(|e| {
                ModelError::Backend(BackendError::ConnectionFailed {
                    backend: "ollama".to_string(),
                    url: self.base_url.clone(),
                    reason: e.to_string(),
                })
            })
        })
        .await
    }

    /// Run `op` under the transient-HTTP retry policy; if it still ends in
    /// `ConnectionFailed` and the server is local, start it
    /// (`ollama::ensure_running`) and run one more retry round. The
    /// "it just works" contract: a dead local server is mermaid's problem,
    /// not the user's. When auto-start itself fails, its hint is appended to
    /// the connection error so the surfaced message says what to do next;
    /// non-local URLs pass their error through untouched.
    ///
    /// `sink` carries the moment-of-spawn notice ("Starting the local
    /// Ollama server…") out as a `StreamEvent::Status` — the revival can
    /// block ~15s behind an otherwise generic spinner, and the spawned
    /// server outlives mermaid, so this one line covers latency feedback,
    /// discoverability, and consent at once. `ensure_running` invokes it
    /// only when a spawn is actually committed (never on `NotLocal` /
    /// Disabled / already-healthy / binary-missing), so no false notices
    /// reach the user.
    ///
    /// The first round deliberately does NOT retry a refused connection.
    /// Backing off and asking a closed port again cannot succeed — only
    /// `ensure_running` can — and on Windows a refused loopback connect costs
    /// ~2s (the SYN is retransmitted before `WSAECONNREFUSED`), so retrying it
    /// three times bought ~9s of dead wait ahead of both the recovery and the
    /// enumeration verbs, which pass `autostart: false` and want the dead state
    /// reported promptly. 5xx and 429 from a server that IS running still
    /// retry, here and in the post-recovery round.
    async fn with_local_recovery<F, Fut>(
        &self,
        sink: Option<&StreamSink>,
        mut op: F,
    ) -> Result<reqwest::Response>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<reqwest::Response>>,
    {
        let first = crate::models::retry::retry_transient_http_no_connect_retry(&mut op).await;
        let Some(recovery) = self.recovery.as_ref() else {
            return first;
        };
        if !matches!(
            first,
            Err(ModelError::Backend(BackendError::ConnectionFailed { .. }))
        ) {
            return first;
        }
        // The turn's sink first (chat), the constructor's console printer
        // second (list paths) — whichever exists carries the one notice.
        //
        // `try_send` and not an awaited send because `ensure_running` takes a
        // synchronous `&str` notifier: the notice has to reach the user
        // *during* a spawn that can block ~15s, not after it. Dropping it is
        // acceptable and unreachable in practice — this fires before the
        // stream produces anything, so the bounded channel is empty, and a
        // status line is best-effort plumbing rather than response content.
        let ensured = match (sink, self.status_notify.as_ref()) {
            (Some(sink), _) => {
                let forward = |text: &str| {
                    let _ = sink.try_send(StreamEvent::Status(text.to_string()));
                };
                recovery
                    .ensure_running(&self.base_url, Some(&forward))
                    .await
            },
            (None, Some(notify)) => {
                let forward = |text: &str| notify(text);
                recovery
                    .ensure_running(&self.base_url, Some(&forward))
                    .await
            },
            (None, None) => recovery.ensure_running(&self.base_url, None).await,
        };
        match ensured {
            Ok(()) => crate::models::retry::retry_transient_http(&mut op).await,
            Err(Some(hint)) => first.map_err(|e| append_reason_hint(e, &hint)),
            Err(None) => first,
        }
    }

    /// Decode the single non-streaming response body into a `ModelResponse`.
    /// Used by both `chat` (no callback) and `chat_typed` (no callback).
    async fn decode_non_streaming(&self, response: reqwest::Response) -> Result<ModelResponse> {
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let debug =
                crate::models::error::ResponseDebugContext::from_headers(response.headers());
            let error_text = error_body(response, "Unknown error").await;
            return Err(ModelError::Backend(BackendError::HttpError {
                status,
                message: error_text,
                debug,
            }));
        }

        let json: OllamaStreamChunk =
            response.json().await.map_err(|e| ModelError::ParseError {
                message: format!("Failed to parse response: {e}"),
                raw: None,
            })?;

        let thinking = json.message.thinking.filter(|t| !t.is_empty());
        let tool_calls = json.message.tool_calls.filter(|tc| !tc.is_empty());

        let prompt_tokens = json.prompt_eval_count.unwrap_or(0);
        let completion_tokens = json.eval_count.unwrap_or(0);

        Ok(ModelResponse {
            content: json.message.content,
            usage: Some(TokenUsage::provider(prompt_tokens, completion_tokens)),
            model_name: self.model_name.clone(),
            stop_reason: json.done_reason.as_deref().map(map_ollama_done_reason),
            thinking,
            tool_calls,
            provider_continuation: None,
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

        // Recovery here is what makes a cold boot self-heal: the startup
        // model check (`ollama::installer`) and the model picker both land on
        // this call, so a dead local server is revived before the first chat.
        // No stream callback exists on this path (notify: None) — its callers
        // are console contexts where the pause reads as startup work.
        let response = self
            .with_local_recovery(None, || async {
                self.client.get(&url).send().await.map_err(|e| {
                    ModelError::Backend(BackendError::ConnectionFailed {
                        backend: "ollama".to_string(),
                        url: self.base_url.clone(),
                        reason: e.to_string(),
                    })
                })
            })
            .await?;

        if !response.status().is_success() {
            return Err(ModelError::Backend(BackendError::HttpError {
                status: response.status().as_u16(),
                message: "Failed to list models".to_string(),
                debug: crate::models::error::ResponseDebugContext::from_headers(response.headers()),
            }));
        }

        let tags: OllamaTagsResponse =
            response.json().await.map_err(|e| ModelError::ParseError {
                message: format!("Failed to parse tags response: {e}"),
                raw: None,
            })?;

        Ok(tags.models.into_iter().map(|m| m.name).collect())
    }

    async fn chat(
        &self,
        messages: &[ChatMessage],
        config: &ModelConfig,
        sink: Option<StreamSink>,
    ) -> Result<ModelResponse> {
        let stream = sink.is_some();
        let supports_thinking = self.thinking_supported().await;
        let request_body = self.build_request_body(messages, config, stream, supports_thinking);
        // The sink doubles as the autostart notice channel: if the local
        // server has to be started, the user sees a status line instead of
        // ~15s of bare spinner.
        let response = self.send_chat(&request_body, sink.as_ref()).await?;

        if let Some(sink) = sink {
            self.handle_stream(response, Some(&sink)).await
        } else {
            self.decode_non_streaming(response).await
        }
    }
}

/// Ollama's wire format as a [`StreamProtocol`].
///
/// The only NDJSON one: frames are newline-delimited JSON objects rather
/// than SSE events, which is also why it is the only one whose framing
/// flushes an un-terminated tail — Ollama can close the body directly on
/// its final object. That used to be a special case written out here and
/// missing from the other three; it is [`Framing::Ndjson`] now.
pub(crate) struct OllamaStream {
    model_name: String,
    acc: StreamAccumulator,
}

impl OllamaStream {
    pub(crate) const fn new(model_name: String) -> Self {
        Self {
            model_name,
            acc: StreamAccumulator {
                content: CappedText::new(),
                thinking: CappedText::new(),
                tool_calls: Vec::new(),
                prompt_tokens: 0,
                completion_tokens: 0,
                saw_usage: false,
                done_reason: None,
                saw_done: false,
            },
        }
    }
}

impl StreamProtocol for OllamaStream {
    const FRAMING: Framing = Framing::Ndjson;

    fn on_frame(&mut self, frame: &str, out: &mut Vec<StreamEvent>) -> Result<Flow> {
        let json_chunk = parse_ollama_stream_frame(frame)?;
        OllamaAdapter::process_stream_chunk(&json_chunk, out, &mut self.acc);
        Ok(Flow::Continue)
    }

    fn finish(self, _out: &mut Vec<StreamEvent>) -> Result<ModelResponse> {
        // F56: a stream that ended before Ollama's terminal `done` chunk was
        // dropped mid-response. Surface it as a stream error instead of a clean
        // `Ok` that's indistinguishable from a real completion. Keyed off the
        // `done` frame (not `done_reason`), so a context-full truncation — a
        // real `done` with `done_reason: "length"`, recovered via
        // compact-and-continue — is preserved, not misclassified.
        if self.acc.closed_abnormally() {
            return Err(ModelError::StreamError(
                "Ollama stream closed before the terminal `done` chunk; the \
                 connection was likely dropped mid-response"
                    .to_string(),
            ));
        }

        // `None` when the stream never reported eval counts, so the reducer keeps
        // its estimate instead of resetting the context gauge to zero (F54).
        // Computed before the fields move below so `usage()` can borrow `acc`.
        let usage = self.acc.usage();
        let stop_reason = self.acc.done_reason.as_deref().map(map_ollama_done_reason);
        let thinking = if self.acc.thinking.is_empty() {
            None
        } else {
            Some(self.acc.thinking.into_string())
        };
        let tool_calls = if self.acc.tool_calls.is_empty() {
            None
        } else {
            Some(self.acc.tool_calls)
        };

        // F3: the adapter never emits a terminal `Done`. The provider
        // wrapper (`providers::model::*`) emits the authoritative
        // `StreamEvent::Done { usage, provider_continuation, stop_reason }`
        // from the returned `ModelResponse`; emitting one here would race it
        // and drop the provider_continuation for Anthropic.

        Ok(ModelResponse {
            content: self.acc.content.into_string(),
            usage,
            model_name: self.model_name,
            stop_reason,
            thinking,
            tool_calls,
            provider_continuation: None,
        })
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
    #[serde(default)]
    done_reason: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct OllamaMessage {
    role: String,
    // F55: a frame may omit `content` (vs sending `""`) — e.g. a thinking-only
    // or tool-call-only delta. Without `default` the whole-chunk parse fails
    // ("missing field content") and tears down the entire stream, matching the
    // `thinking`/`tool_calls` siblings which already default.
    #[serde(default)]
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
    /// On-disk (quantized) byte size — closely approximates the VRAM weight
    /// footprint, which auto-sizing subtracts from the memory budget.
    #[serde(default)]
    pub(crate) size: Option<u64>,
}

/// Subset of the `/api/show` response we parse — only `model_info` (the
/// architecture-prefixed dimensions). Byte size is NOT here; it comes from
/// `/api/tags`.
#[derive(Debug, Deserialize)]
struct OllamaShowResponse {
    #[serde(default)]
    model_info: serde_json::Value,
    /// Advertised capabilities (`completion`, `tools`, `thinking`, `vision`, …).
    /// Absent on older Ollama; used to gate the `think` field (#122).
    #[serde(default)]
    capabilities: Vec<String>,
}

/// `/api/ps` response — currently-loaded models and their memory placement.
/// Extra fields (`digest`, `expires_at`, `details`, …) are ignored.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct OllamaPsResponse {
    #[serde(default)]
    pub(crate) models: Vec<OllamaPsModel>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct OllamaPsModel {
    pub(crate) name: String,
    /// Total bytes the loaded model occupies (weights + KV + buffers).
    #[serde(default)]
    pub(crate) size: Option<u64>,
    /// Of that, the bytes resident in VRAM. Less than `size` ⇒ partial offload.
    #[serde(default)]
    pub(crate) size_vram: Option<u64>,
}

/// Capabilities probed from `/api/show` (+ `/api/tags` for the weight size),
/// used to auto-size `num_ctx`. All fields are best-effort and independently
/// optional. Serialized into the `provider_probes` cache so subsequent sessions
/// skip the probe.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OllamaModelInfo {
    /// The model's architectural max context window.
    pub context_length: Option<usize>,
    /// Architecture dimensions for the KV-cache estimate.
    pub dims: Option<ModelDims>,
    /// On-disk (quantized) weight bytes.
    pub weight_bytes: Option<u64>,
}

// Helper functions

/// Append an auto-start hint to a `ConnectionFailed` reason so the surfaced
/// error explains what mermaid tried and what the user can do ("Ollama isn't
/// installed — …"). Other error shapes pass through untouched.
fn append_reason_hint(error: ModelError, hint: &str) -> ModelError {
    match error {
        ModelError::Backend(BackendError::ConnectionFailed {
            backend,
            url,
            reason,
        }) => ModelError::Backend(BackendError::ConnectionFailed {
            backend,
            url,
            reason: format!("{reason}. {hint}"),
        }),
        other => other,
    }
}

/// Parse one newline-delimited Ollama stream frame into an `OllamaStreamChunk`.
///
/// F53: a mid-stream `{"error":"..."}` frame lacks the `message`/`done` fields
/// of `OllamaStreamChunk`, so a direct typed parse fails with a generic
/// `ParseError("missing field `message`")` and the real provider error survives
/// only inside `raw`. Check for a top-level `error` string first and surface it
/// as a typed `ProviderError` (mirrors `openai_compat.rs` / gemini.rs stream
/// paths) before falling back to the typed-chunk parse.
fn parse_ollama_stream_frame(line: &str) -> Result<OllamaStreamChunk> {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(line)
        && let Some(message) = value.get("error").and_then(|v| v.as_str())
    {
        return Err(ModelError::Backend(BackendError::ProviderError {
            provider: "ollama".to_string(),
            code: None,
            message: message.to_string(),
            debug: crate::models::error::ResponseDebugContext::default(),
        }));
    }
    serde_json::from_str(line).map_err(|e| ModelError::ParseError {
        message: format!("Failed to parse Ollama response: {e}"),
        raw: Some(line.to_string()),
    })
}

/// Map Ollama's `done_reason` to the shared `FinishReason`. Ollama emits `"stop"`
/// (natural end) and `"length"` (hit `num_predict`/context); anything else
/// (operational reasons like `"load"`) is preserved via `Other` so it still
/// surfaces rather than being dropped to `None` (#13).
fn map_ollama_done_reason(s: &str) -> FinishReason {
    match s {
        "stop" => FinishReason::Stop,
        "length" => FinishReason::Length,
        other => FinishReason::Other(other.to_string()),
    }
}

/// Coerce a `model_info` JSON value to `usize` (the dimension keys are integers).
fn json_to_usize(v: &serde_json::Value) -> Option<usize> {
    v.as_u64().map(|n| n as usize)
}

/// The model's max context window from `/api/show` `model_info`. Keys are
/// architecture-prefixed (`qwen2.context_length`, `llama.context_length`,
/// `gptoss.context_length`, …); we prefer the prefix named by
/// `general.architecture`, then fall back to any key ending in `.context_length`.
/// Generic so a new architecture needs no code change.
fn context_length_from_model_info(model_info: &serde_json::Value) -> Option<usize> {
    let obj = model_info.as_object()?;
    if let Some(arch) = obj.get("general.architecture").and_then(|v| v.as_str())
        && let Some(v) = obj
            .get(&format!("{arch}.context_length"))
            .and_then(json_to_usize)
    {
        return Some(v);
    }
    obj.iter()
        .find(|(k, _)| k.ends_with(".context_length"))
        .and_then(|(_, v)| json_to_usize(v))
}

/// Architecture dimensions for the KV-cache estimate, by arch-suffixed key.
/// `head_count_kv` defaults to `head_count` when absent (non-GQA models).
/// Returns `None` if any required dimension is missing.
fn dims_from_model_info(model_info: &serde_json::Value) -> Option<ModelDims> {
    let obj = model_info.as_object()?;
    let by_suffix = |suffix: &str| -> Option<usize> {
        obj.iter()
            .find(|(k, _)| k.ends_with(suffix))
            .and_then(|(_, v)| json_to_usize(v))
    };
    let head_count = by_suffix(".attention.head_count")?;
    Some(ModelDims {
        block_count: by_suffix(".block_count")?,
        head_count,
        head_count_kv: by_suffix(".attention.head_count_kv").unwrap_or(head_count),
        embedding_length: by_suffix(".embedding_length")?,
    })
}

fn normalize_url(url: &str) -> String {
    let mut normalized = url.trim().to_string();

    // Replace 0.0.0.0 with 127.0.0.1
    if normalized.contains("0.0.0.0") {
        normalized = normalized.replace("0.0.0.0", "127.0.0.1");
    }

    // Add a scheme if missing, chosen by host class: loopback / private / LAN
    // hosts may use cleartext http, but a public host defaults to https so
    // prompt data isn't sent over the open internet in the clear (#86). An
    // explicit scheme (http or https) is always respected. Mirrors the factory's
    // `validate_provider_base_url` gate.
    if !normalized.starts_with("http://") && !normalized.starts_with("https://") {
        let host = normalized.split(['/', ':']).next().unwrap_or("");
        let scheme = if crate::utils::classify_host(host).is_internal() {
            "http"
        } else {
            "https"
        };
        normalized = format!("{scheme}://{normalized}");
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
            normalized = format!("http://{authority}:11434{path}");
        }
    }
    // For https:// without a port, don't add :11434 — the default port (443) is correct

    normalized
}

#[cfg(test)]
mod tests {
    use super::super::accumulator::CappedText;
    use super::{normalize_url, uses_effort_string_think};

    // --- /api/ps placement parsing (mirrors model_placement's selection) ---

    #[test]
    fn ps_response_selects_model_and_handles_missing_fields() {
        // Realistic body: a partially-offloaded model, the target fully on GPU,
        // one entry missing size_vram, plus extra fields we must ignore.
        let body = serde_json::json!({
            "models": [
                { "name": "other:7b", "size": 8_000_000_000u64, "size_vram": 4_000_000_000u64,
                  "digest": "abc", "expires_at": "2026-01-01T00:00:00Z" },
                { "name": "ornith:9b", "size": 6_000_000_000u64, "size_vram": 6_000_000_000u64 },
                { "name": "nogpu:1b", "size": 1_000_000_000u64 },
            ]
        });
        let ps: super::OllamaPsResponse = serde_json::from_value(body).unwrap();
        // Same selection model_placement does: find by name, require both bytes.
        let pick = |name: &str| {
            ps.models
                .iter()
                .find(|m| m.name == name)
                .and_then(|m| Some((m.size_vram?, m.size?)))
        };
        assert_eq!(pick("ornith:9b"), Some((6_000_000_000, 6_000_000_000)));
        assert_eq!(pick("other:7b"), Some((4_000_000_000, 8_000_000_000)));
        assert_eq!(pick("nogpu:1b"), None); // missing size_vram → None
        assert_eq!(pick("absent:1b"), None); // not loaded → None
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

    #[test]
    fn normalize_url_public_host_defaults_to_https() {
        // #86: a scheme-less public host must not be addressed over cleartext.
        assert_eq!(
            normalize_url("my-remote-ollama.com:11434"),
            "https://my-remote-ollama.com:11434"
        );
    }

    #[test]
    fn normalize_url_private_host_stays_http() {
        // Loopback / LAN hosts keep cleartext http (no port → :11434 added).
        assert_eq!(normalize_url("192.168.1.50"), "http://192.168.1.50:11434");
        assert_eq!(normalize_url("127.0.0.1:11434"), "http://127.0.0.1:11434");
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
    async fn connection_failure_passes_through_when_autostart_disabled() {
        // Reserve a port, then release it so nothing is listening. With
        // autostart disabled the adapter must surface the plain connection
        // error — no `ensure_running` attempt, no hint injected. (The
        // autostart=true path is deliberately not exercised here: on a dev
        // box it would spawn a real `ollama serve`.)
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            listener.local_addr().expect("addr").port()
        };
        let backend = BackendConfig {
            ollama_url: format!("http://127.0.0.1:{port}"),
            timeout_secs: 1,
            max_idle_per_host: 1,
            ollama_autostart: false,
        };
        use crate::models::traits::Model;
        let adapter = OllamaAdapter::new("test-model", Arc::new(backend))
            .await
            .expect("adapter");
        let err = adapter
            .list_models()
            .await
            .expect_err("dead port must fail");
        let msg = err.to_string();
        assert!(msg.contains("Failed to connect to ollama"), "got: {msg}");
        assert!(
            !msg.contains("auto-start") && !msg.contains("ollama.com/download"),
            "no hint expected with autostart disabled, got: {msg}"
        );
    }

    #[test]
    fn append_reason_hint_enriches_connection_failed_only() {
        use crate::models::error::{BackendError, ModelError};
        let base = ModelError::Backend(BackendError::ConnectionFailed {
            backend: "ollama".into(),
            url: "http://localhost:11434".into(),
            reason: "connection refused".into(),
        });
        let enriched = super::append_reason_hint(base, "install it from https://ollama.com");
        assert!(
            enriched
                .to_string()
                .contains("connection refused. install it from https://ollama.com"),
            "got: {enriched}"
        );
        // Non-connection errors pass through untouched.
        let other = ModelError::ParseError {
            message: "bad json".into(),
            raw: None,
        };
        let untouched = super::append_reason_hint(other, "should not appear");
        assert!(!untouched.to_string().contains("should not appear"));
    }

    /// Adapter contract (see `MessageAudience`): harness steering must reach
    /// the model. Ollama carries a native system role in history, so the
    /// reminder passes through at the TAIL — the position weak local models
    /// were observed to actually read.
    #[tokio::test]
    async fn model_directed_system_messages_reach_the_wire_in_place() {
        use crate::models::ChatMessageKind;
        let adapter = make_adapter().await;
        let mut nudge = ChatMessage::system("Reminder: plan mode is active.");
        nudge.kind = ChatMessageKind::RecoveryNudge;
        let messages = vec![ChatMessage::user("ok"), nudge];
        let body = adapter.build_request_body(&messages, &ModelConfig::default(), false, false);

        let msgs = body["messages"].as_array().expect("messages array");
        let last = msgs.last().expect("non-empty");
        assert_eq!(last["role"], "system");
        assert!(
            last["content"]
                .as_str()
                .unwrap()
                .contains("plan mode is active"),
        );
    }

    #[tokio::test]
    async fn ollama_request_body_omits_think_when_reasoning_none() {
        let adapter = make_adapter().await;
        let config = ModelConfig {
            reasoning: ReasoningLevel::None,
            ..Default::default()
        };
        let messages = vec![ChatMessage::user("hi")];

        let body = adapter.build_request_body(&messages, &config, false, true);
        assert_eq!(body["think"], serde_json::json!(false));
    }

    #[tokio::test]
    async fn ollama_request_body_preserves_registry_selected_web_tools() {
        let adapter = make_adapter().await;
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

        let body = adapter.build_request_body(&[ChatMessage::user("hi")], &config, false, false);
        let names: Vec<&str> = body["tools"]
            .as_array()
            .expect("tools array")
            .iter()
            .filter_map(|tool| {
                tool.pointer("/function/name")
                    .and_then(serde_json::Value::as_str)
            })
            .collect();
        assert_eq!(names, ["web_fetch", "web_search"]);
    }

    #[tokio::test]
    async fn ollama_request_body_sets_think_true_for_low_reasoning() {
        let adapter = make_adapter().await;
        let config = ModelConfig {
            reasoning: ReasoningLevel::Low,
            ..Default::default()
        };
        let messages = vec![ChatMessage::user("hi")];

        let body = adapter.build_request_body(&messages, &config, false, true);
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

        let body = adapter.build_request_body(&messages, &config, false, true);
        assert_eq!(body["think"], serde_json::json!(true));
    }

    #[tokio::test]
    async fn ollama_request_body_omits_think_when_unsupported() {
        // #122: a model that doesn't advertise the `thinking` capability must
        // not receive a `think` field at all — recent Ollama 400s on it.
        let adapter = make_adapter().await;
        let config = ModelConfig {
            reasoning: ReasoningLevel::High,
            ..Default::default()
        };
        let messages = vec![ChatMessage::user("hi")];

        let body = adapter.build_request_body(&messages, &config, false, false);
        assert!(
            body.get("think").is_none(),
            "think must be omitted for a non-thinking model, got {:?}",
            body.get("think")
        );
    }

    #[tokio::test]
    async fn ollama_request_body_emits_num_ctx_and_num_predict() {
        let adapter = make_adapter().await;
        let mut config = ModelConfig::default();
        config.set_backend_option("ollama".into(), "num_ctx".into(), "32768".into());
        config.set_backend_option("ollama".into(), "num_predict".into(), "8192".into());

        let body = adapter.build_request_body(&[ChatMessage::user("hi")], &config, false, true);
        assert_eq!(body["options"]["num_ctx"], serde_json::json!(32768));
        assert_eq!(body["options"]["num_predict"], serde_json::json!(8192));
    }

    #[tokio::test]
    async fn ollama_request_body_omits_sizing_when_unset() {
        let adapter = make_adapter().await;
        let config = ModelConfig::default();
        let body = adapter.build_request_body(&[ChatMessage::user("hi")], &config, false, true);
        // Unset → omitted entirely so Ollama uses its own defaults.
        assert!(body["options"].get("num_ctx").is_none());
        assert!(body["options"].get("num_predict").is_none());
    }

    // --- /api/show model_info parsing (context window + dims) ---

    #[test]
    fn context_length_prefers_architecture_prefix() {
        let mi = serde_json::json!({
            "general.architecture": "qwen2",
            "qwen2.context_length": 262_144,
            "qwen2.block_count": 28,
        });
        assert_eq!(super::context_length_from_model_info(&mi), Some(262_144));
    }

    #[test]
    fn context_length_falls_back_to_any_suffix() {
        // No general.architecture, but a *.context_length key exists.
        let mi = serde_json::json!({ "llama.context_length": 131_072 });
        assert_eq!(super::context_length_from_model_info(&mi), Some(131_072));
    }

    #[test]
    fn context_length_missing_is_none() {
        let mi = serde_json::json!({ "general.architecture": "qwen2" });
        assert_eq!(super::context_length_from_model_info(&mi), None);
    }

    #[test]
    fn dims_parsed_for_gqa_model() {
        let mi = serde_json::json!({
            "general.architecture": "qwen2",
            "qwen2.block_count": 28,
            "qwen2.attention.head_count": 28,
            "qwen2.attention.head_count_kv": 4,
            "qwen2.embedding_length": 3584,
        });
        let dims = super::dims_from_model_info(&mi).unwrap();
        assert_eq!(dims.block_count, 28);
        assert_eq!(dims.head_count, 28);
        assert_eq!(dims.head_count_kv, 4);
        assert_eq!(dims.embedding_length, 3584);
    }

    #[test]
    fn dims_head_count_kv_defaults_to_head_count() {
        // Non-GQA model: no head_count_kv key → assume KV heads == heads.
        let mi = serde_json::json!({
            "llama.block_count": 32,
            "llama.attention.head_count": 32,
            "llama.embedding_length": 4096,
        });
        let dims = super::dims_from_model_info(&mi).unwrap();
        assert_eq!(dims.head_count_kv, 32);
    }

    #[test]
    fn dims_missing_required_is_none() {
        let mi = serde_json::json!({ "gptoss.block_count": 24 }); // missing the rest
        assert!(super::dims_from_model_info(&mi).is_none());
    }

    #[test]
    fn gptoss_architecture_prefix_parsed() {
        let mi = serde_json::json!({
            "general.architecture": "gptoss",
            "gptoss.context_length": 131_072,
            "gptoss.block_count": 24,
            "gptoss.attention.head_count": 64,
            "gptoss.attention.head_count_kv": 8,
            "gptoss.embedding_length": 2880,
        });
        assert_eq!(super::context_length_from_model_info(&mi), Some(131_072));
        assert!(super::dims_from_model_info(&mi).is_some());
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
    async fn ollama_request_body_maps_output_schema_to_format() {
        let adapter = make_adapter().await;
        let config = ModelConfig {
            output_schema: Some(serde_json::json!({"type": "object"})),
            ..Default::default()
        };
        let body = adapter.build_request_body(&[ChatMessage::user("hi")], &config, false, true);
        assert_eq!(body["format"]["type"], "object");
        // Absent -> no format key.
        let body = adapter.build_request_body(
            &[ChatMessage::user("hi")],
            &ModelConfig::default(),
            false,
            true,
        );
        assert!(body.get("format").is_none());
    }

    #[tokio::test]
    async fn ollama_request_body_sets_think_low_for_gpt_oss_none() {
        let adapter = make_gpt_oss_adapter().await;
        let config = ModelConfig {
            reasoning: ReasoningLevel::None,
            ..Default::default()
        };
        let body = adapter.build_request_body(&[ChatMessage::user("hi")], &config, false, true);
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
        let body = adapter.build_request_body(&[ChatMessage::user("hi")], &config, false, true);
        assert_eq!(body["think"], serde_json::json!("medium"));
    }

    #[tokio::test]
    async fn ollama_request_body_sets_think_high_for_gpt_oss_max() {
        let adapter = make_gpt_oss_adapter().await;
        let config = ModelConfig {
            reasoning: ReasoningLevel::Max,
            ..Default::default()
        };
        let body = adapter.build_request_body(&[ChatMessage::user("hi")], &config, false, true);
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
        let body = adapter.build_request_body(&[ChatMessage::user("hi")], &config, false, true);
        assert_eq!(body["think"], serde_json::json!("high"));
    }

    #[test]
    fn gpt_oss_effort_string_matches_prefix_case_insensitive() {
        assert!(uses_effort_string_think("gpt-oss:20b"));
        assert!(uses_effort_string_think("gpt-oss:120b-cloud"));
        assert!(uses_effort_string_think("GPT-OSS:20b"));
        assert!(!uses_effort_string_think("qwen3-coder:30b"));
        assert!(!uses_effort_string_think("gpt-4o"));
    }

    #[test]
    fn map_ollama_done_reason_maps_known_and_preserves_unknown() {
        use super::{FinishReason, map_ollama_done_reason};
        assert_eq!(map_ollama_done_reason("stop"), FinishReason::Stop);
        assert_eq!(map_ollama_done_reason("length"), FinishReason::Length);
        assert_eq!(
            map_ollama_done_reason("load"),
            FinishReason::Other("load".to_string())
        );
    }

    #[test]
    fn process_stream_chunk_captures_done_reason_and_saturates_tokens() {
        // #13: the terminal chunk's done_reason is recorded (was hardcoded None).
        // #49: token totals use saturating_add (here near usize::MAX).
        use super::{OllamaMessage, OllamaStreamChunk, StreamAccumulator};
        let mut acc = StreamAccumulator {
            content: CappedText::new(),
            thinking: CappedText::new(),
            tool_calls: Vec::new(),
            prompt_tokens: 0,
            completion_tokens: 0,
            saw_usage: false,
            done_reason: None,
            saw_done: false,
        };
        let chunk = OllamaStreamChunk {
            message: OllamaMessage {
                role: "assistant".to_string(),
                content: String::new(),
                thinking: None,
                tool_calls: None,
            },
            done: true,
            prompt_eval_count: Some(usize::MAX),
            eval_count: Some(10),
            done_reason: Some("length".to_string()),
        };
        OllamaAdapter::process_stream_chunk(&chunk, &mut Vec::new(), &mut acc);
        assert_eq!(acc.done_reason.as_deref(), Some("length"));
        assert_eq!(acc.prompt_tokens, usize::MAX);
        assert_eq!(acc.completion_tokens, 10);
        // F54: real eval counts arrived → usage is reported (not None).
        assert!(acc.saw_usage);
        assert!(acc.usage().is_some());
        // #49: the total saturates instead of wrapping/panicking.
        assert_eq!(
            acc.prompt_tokens.saturating_add(acc.completion_tokens),
            usize::MAX
        );
    }

    fn empty_accumulator() -> super::StreamAccumulator {
        super::StreamAccumulator {
            content: CappedText::new(),
            thinking: CappedText::new(),
            tool_calls: Vec::new(),
            prompt_tokens: 0,
            completion_tokens: 0,
            saw_usage: false,
            done_reason: None,
            saw_done: false,
        }
    }

    #[test]
    fn stream_usage_is_none_when_counts_absent_then_some_after_done() {
        // F54: a stream cut before the terminal `done` chunk (no eval counts)
        // must report `None` usage so the reducer keeps its estimate instead of
        // resetting the context gauge to zero. A real `done` flips it to `Some`.
        use super::{OllamaMessage, OllamaStreamChunk};
        let mut acc = empty_accumulator();

        let content_chunk = OllamaStreamChunk {
            message: OllamaMessage {
                role: "assistant".to_string(),
                content: "hi".to_string(),
                thinking: None,
                tool_calls: None,
            },
            done: false,
            prompt_eval_count: None,
            eval_count: None,
            done_reason: None,
        };
        OllamaAdapter::process_stream_chunk(&content_chunk, &mut Vec::new(), &mut acc);
        assert!(
            acc.usage().is_none(),
            "a cut stream must not reset the gauge to a zero usage"
        );

        let done_chunk = OllamaStreamChunk {
            message: OllamaMessage {
                role: "assistant".to_string(),
                content: String::new(),
                thinking: None,
                tool_calls: None,
            },
            done: true,
            prompt_eval_count: Some(120),
            eval_count: Some(8),
            done_reason: Some("stop".to_string()),
        };
        OllamaAdapter::process_stream_chunk(&done_chunk, &mut Vec::new(), &mut acc);
        let usage = acc
            .usage()
            .expect("usage present after a done chunk with counts");
        assert_eq!(usage.prompt_tokens, 120);
        assert_eq!(usage.completion_tokens, 8);
        assert_eq!(usage.total_tokens(), 128);
    }

    #[test]
    fn closed_abnormally_until_terminal_done_chunk_seen() {
        // F56: a stream is abnormal until Ollama's terminal `done` chunk lands.
        use super::{OllamaMessage, OllamaStreamChunk};
        let mut acc = empty_accumulator();
        // Fresh / before any frame → abnormal (nothing terminal observed yet).
        assert!(acc.closed_abnormally());

        // A content delta (done: false) is NOT the terminal frame.
        let content_chunk = OllamaStreamChunk {
            message: OllamaMessage {
                role: "assistant".to_string(),
                content: "partial".to_string(),
                thinking: None,
                tool_calls: None,
            },
            done: false,
            prompt_eval_count: None,
            eval_count: None,
            done_reason: None,
        };
        OllamaAdapter::process_stream_chunk(&content_chunk, &mut Vec::new(), &mut acc);
        assert!(
            acc.closed_abnormally(),
            "a stream cut before `done` must be flagged abnormal"
        );

        // The terminal `done` chunk flips it to a clean completion.
        let done_chunk = OllamaStreamChunk {
            message: OllamaMessage {
                role: "assistant".to_string(),
                content: String::new(),
                thinking: None,
                tool_calls: None,
            },
            done: true,
            prompt_eval_count: Some(10),
            eval_count: Some(2),
            done_reason: Some("stop".to_string()),
        };
        OllamaAdapter::process_stream_chunk(&done_chunk, &mut Vec::new(), &mut acc);
        assert!(
            !acc.closed_abnormally(),
            "a `done` chunk completes the stream"
        );
    }

    #[test]
    fn context_full_length_truncation_is_not_abnormal() {
        // CRUCIAL truncation-recovery guard: Ollama signals context-full as a
        // CLEAN terminal `done` chunk with `done_reason: "length"`. It must NOT
        // be misclassified as an abnormal close — `saw_done` is set, so the
        // adapter returns Ok with FinishReason::Length for compact-and-continue.
        use super::{FinishReason, OllamaMessage, OllamaStreamChunk, map_ollama_done_reason};
        let mut acc = empty_accumulator();
        let length_done = OllamaStreamChunk {
            message: OllamaMessage {
                role: "assistant".to_string(),
                content: "...".to_string(),
                thinking: None,
                tool_calls: None,
            },
            done: true,
            prompt_eval_count: Some(4096),
            eval_count: Some(512),
            done_reason: Some("length".to_string()),
        };
        OllamaAdapter::process_stream_chunk(&length_done, &mut Vec::new(), &mut acc);
        assert!(
            !acc.closed_abnormally(),
            "context-full Length truncation has a real `done` frame — not abnormal"
        );
        assert_eq!(
            acc.done_reason.as_deref().map(map_ollama_done_reason),
            Some(FinishReason::Length)
        );
    }

    #[test]
    fn stream_frame_error_becomes_typed_provider_error() {
        // F53: a mid-stream `{"error":"..."}` frame must surface as a typed
        // ProviderError carrying the real message, not a generic
        // `ParseError("missing field `message`")`.
        use super::{BackendError, ModelError, parse_ollama_stream_frame};
        let err = parse_ollama_stream_frame(r#"{"error":"model requires more system memory"}"#)
            .expect_err("error frame must not parse as a chunk");
        match err {
            ModelError::Backend(BackendError::ProviderError {
                provider, message, ..
            }) => {
                assert_eq!(provider, "ollama");
                assert_eq!(message, "model requires more system memory");
            },
            other => panic!("expected ProviderError, got {other:?}"),
        }
    }

    #[test]
    fn stream_frame_normal_chunk_still_parses() {
        // The error-frame guard must not disturb a normal content frame.
        use super::parse_ollama_stream_frame;
        let chunk = parse_ollama_stream_frame(
            r#"{"message":{"role":"assistant","content":"hello"},"done":false}"#,
        )
        .expect("normal frame parses");
        assert_eq!(chunk.message.content, "hello");
        assert!(!chunk.done);
    }

    #[test]
    fn ollama_message_defaults_missing_content() {
        // F55: a frame that omits `content` (vs sending `""`) must still parse —
        // `content` defaults to "" rather than tearing down the whole stream.
        let chunk: super::OllamaStreamChunk = serde_json::from_str(
            r#"{"message":{"role":"assistant","thinking":"hmm"},"done":false}"#,
        )
        .expect("frame without content parses");
        assert_eq!(chunk.message.content, "");
        assert_eq!(chunk.message.thinking.as_deref(), Some("hmm"));
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

        let body = adapter.build_request_body(&messages, &config, false, true);
        let messages_arr = body["messages"].as_array().expect("messages array");
        assert_eq!(messages_arr[0]["role"], "system");
        let content = messages_arr[0]["content"].as_str().unwrap();
        assert!(content.contains("You are Mermaid."));
        assert!(content.contains("Project rule: always snake_case."));
        assert!(content.contains("---"));
    }
}
