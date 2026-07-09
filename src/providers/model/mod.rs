//! Model adapters wrapped as `ModelProvider` implementations.
//!
//! Four providers today: Ollama, Anthropic, Gemini, and OpenAI-
//! compat (covering OpenAI, OpenRouter, Groq, Cerebras, DeepInfra,
//! Together, and user-defined endpoints). Each wraps the
//! corresponding adapter in `crate::models::adapters`; the adapter
//! owns the wire format and the wrapper owns the trait shape.

pub mod anthropic;
pub mod gemini;
pub mod ollama;
pub mod openai_compat;
pub(crate) mod stream_bridge;

use std::sync::Arc;

use async_trait::async_trait;

use crate::domain::{ChatRequest, TurnId};
use crate::models::adapters::ModelLimits;
use crate::models::adapters::ollama_sizing::NumCtxSource;
use crate::models::{ModelError, Result, TokenUsage};
use crate::runtime::{NewProviderProbe, RuntimeStore};

use super::capabilities::Capabilities;
use super::ctx::{FinalResponse, StreamContext, StreamEvent};

/// Resolved context sizing for a turn. For most providers `model_max ==
/// effective` (the static advertised window). For Ollama they differ:
/// `model_max` is the probed architectural window, while `effective` is what we
/// actually enforce as `num_ctx` (auto-fitted to memory, capped, or an
/// override). Compaction and the status bar use `effective`; "model supports up
/// to X" uses `model_max`.
#[derive(Debug, Clone, Copy, Default)]
pub struct ContextSizing {
    pub model_max: Option<usize>,
    pub effective: Option<usize>,
    /// How `effective` was chosen (Ollama only). `None` for static/advertised.
    pub source: Option<NumCtxSource>,
    /// The model's per-response output ceiling, when the provider exposes one
    /// (`/models` metadata, or a documented static table). Rides the same
    /// resolve→reducer pipeline as the window so `provider_capabilities` can be
    /// refreshed live.
    pub max_output: Option<usize>,
}

/// Where a loaded model actually sits in memory, from a post-turn probe (Ollama
/// `/api/ps`). `total_bytes` is weights + KV + buffers; `size_vram_bytes` is the
/// part resident in VRAM. `size_vram_bytes < total_bytes` means the model spilled
/// to CPU/RAM (partial offload → slow). Only Ollama reports this.
#[derive(Debug, Clone, Copy)]
pub struct ModelPlacement {
    pub size_vram_bytes: u64,
    pub total_bytes: u64,
    /// Auto-converge target: when the model spilled, the largest `num_ctx` that
    /// would fit instead — or `None` if it already fits or shrinking can't help
    /// (weights-bound). Computed against the *measured* footprint.
    pub suggested_num_ctx: Option<u32>,
}

/// Provider-facing interface. A `ModelProvider` impl owns whatever
/// HTTP client / state it needs and exposes `chat()` — that's the
/// whole surface.
#[async_trait]
pub trait ModelProvider: Send + Sync {
    /// Capabilities the provider advertises. The reducer reads this
    /// when building the outgoing `ChatRequest` (e.g. whether to
    /// attach reasoning controls).
    fn capabilities(&self) -> &Capabilities;

    /// Resolve the *effective* context window for a turn (what the model will
    /// actually enforce). The default returns the static advertised window;
    /// Ollama overrides this to probe the model's real window and auto-fit
    /// `num_ctx` to host memory, honoring the request's per-model
    /// `ollama_num_ctx` override. `None` means "let the backend decide".
    ///
    /// Awaited only on the effect runtime (never the reducer), so a probe never
    /// blocks the UI.
    async fn resolve_context_window(&self, request: &ChatRequest) -> ContextSizing {
        let _ = request;
        let max = self.capabilities().max_context_tokens;
        ContextSizing {
            model_max: max,
            effective: max,
            source: None,
            max_output: self.capabilities().max_output_tokens,
        }
    }

    /// Best-effort: where the loaded model currently sits in memory. The default
    /// returns `None` (unknown / not applicable); Ollama overrides it to probe
    /// `/api/ps`. Awaited only on the effect runtime, *after* a turn (when the
    /// model is resident), so it never blocks the UI.
    async fn verify_placement(&self, current_num_ctx: Option<usize>) -> Option<ModelPlacement> {
        let _ = current_num_ctx;
        None
    }

    /// Best-effort: whether the active model can actually see images. `None`
    /// means "unknown / not applicable" — the default for providers that don't
    /// probe, and for cloud providers whose vision support is already known
    /// good; `Some(false)` is what drives the no-vision-model warning. Awaited
    /// only on the effect runtime, so a probe never blocks the UI.
    async fn supports_vision(&self) -> Option<bool> {
        None
    }

    /// Stream a chat turn. Typed events flow through
    /// `ctx.sink`; the returned `FinalResponse` carries token usage
    /// and the Anthropic thinking-signature (opaque blob required to
    /// continue extended thinking across turns).
    ///
    /// Cancellation: the provider MUST select! on `ctx.token.
    /// cancelled()` inside any await that could block for more than
    /// a few hundred ms. This is the contract that replaces the old
    /// `check_interrupt` polling pattern.
    async fn chat(&self, request: ChatRequest, ctx: StreamContext) -> Result<FinalResponse>;
}

/// Run a one-shot, non-interactive model call and collect its streamed text
/// into a `String`. For internal calls whose output is NOT shown to the user
/// as it streams — context compaction and the Auto-mode safety classifier.
/// Drains a private event channel (ignoring reasoning / tool-call events) and
/// returns the collected text plus final token usage. The `token` lets the
/// caller cancel the call (e.g. on Ctrl+C) like any other turn work.
pub(crate) async fn collect_text(
    provider: Arc<dyn ModelProvider>,
    turn: TurnId,
    request: ChatRequest,
    token: tokio_util::sync::CancellationToken,
) -> Result<(String, Option<TokenUsage>)> {
    let (stream_tx, mut stream_rx) = tokio::sync::mpsc::channel::<StreamEvent>(128);
    let ctx = StreamContext::new(token, stream_tx, turn);
    let collector = tokio::task::spawn(async move {
        let mut text = String::new();
        let mut usage = None;
        while let Some(event) = stream_rx.recv().await {
            match event {
                StreamEvent::Text(chunk) => text.push_str(&chunk),
                StreamEvent::Done {
                    usage: done_usage, ..
                } => usage = done_usage,
                // Status is a user-facing plumbing notice, not content —
                // a text collector has nowhere to surface it.
                StreamEvent::Reasoning(_)
                | StreamEvent::ToolCall(_)
                | StreamEvent::Status(_)
                | StreamEvent::ThinkingSignature(_) => {},
            }
        }
        (text, usage)
    });

    let response = provider.chat(request, ctx).await;
    let (text, stream_usage) = collector.await.map_err(|err| {
        ModelError::StreamError(format!("collect_text collector failed: {}", err))
    })?;
    match response {
        Ok(final_response) => Ok((text, final_response.usage.or(stream_usage))),
        Err(err) => Err(err),
    }
}

pub use anthropic::AnthropicProvider;
pub use gemini::GeminiProvider;
pub use ollama::OllamaProvider;
pub use openai_compat::OpenAICompatProvider;

/// True when a `provider_probes` row is older than the probe TTL (shared by
/// the Ollama context probe and the per-provider limits probes). An
/// unparseable timestamp is treated as stale so it re-probes.
pub(crate) fn probe_is_stale(probed_at: &str) -> bool {
    use chrono::{DateTime, Utc};
    match DateTime::parse_from_rfc3339(probed_at) {
        Ok(t) => {
            Utc::now()
                .signed_duration_since(t.with_timezone(&Utc))
                .num_days()
                >= crate::constants::PROVIDER_PROBE_TTL_DAYS
        },
        // Unparseable timestamp → treat as stale and re-probe.
        Err(_) => true,
    }
}

/// Limits learned from one live probe of a provider's models endpoint, cached
/// per (provider, model) in `provider_probes`. A successful fetch WITHOUT
/// limit metadata (or a definitive "model not listed") is cached too — as
/// `None`s — so providers that don't expose limits aren't re-fetched every
/// turn. Fetch *failures* are never cached.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct CachedLimits {
    pub(crate) max_context_tokens: Option<usize>,
    pub(crate) max_output_tokens: Option<usize>,
}

pub(crate) const LIMITS_PROBE_KEY: &str = "limits_probe";

/// Load fresh cached limits, off the async runtime. Best-effort → `None`.
pub(crate) async fn load_limits_from_db(provider: String, model: String) -> Option<CachedLimits> {
    tokio::task::spawn_blocking(move || {
        let store = RuntimeStore::open_default().ok()?;
        let rec = store
            .provider_probes()
            .get(&provider, &model, LIMITS_PROBE_KEY)
            .ok()??;
        if probe_is_stale(&rec.probed_at) {
            return None;
        }
        serde_json::from_str::<CachedLimits>(&rec.capability_value).ok()
    })
    .await
    .ok()
    .flatten()
}

/// Persist probed limits for subsequent sessions. Best-effort.
pub(crate) async fn save_limits_to_db(provider: String, model: String, limits: &CachedLimits) {
    let value = match serde_json::to_string(limits) {
        Ok(v) => v,
        Err(_) => return,
    };
    let _ = tokio::task::spawn_blocking(move || -> Option<()> {
        let store = RuntimeStore::open_default().ok()?;
        store
            .provider_probes()
            .upsert(NewProviderProbe {
                provider,
                model_id: model,
                capability_key: LIMITS_PROBE_KEY.into(),
                capability_value: value,
                confidence: "probed".into(),
                error: None,
            })
            .ok()?;
        Some(())
    })
    .await;
}

/// Cache-first limit resolution: return fresh cached limits when present,
/// otherwise run `fetch` against the provider's models endpoint. A successful
/// fetch is cached even when all-`None` (definitive "provider exposes
/// nothing"); a failed fetch is NOT cached and resolves to `None` so the next
/// turn retries.
pub(crate) async fn resolve_limits_cached<F, Fut>(
    provider: &str,
    model: &str,
    fetch: F,
) -> Option<CachedLimits>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<ModelLimits>>,
{
    if let Some(cached) = load_limits_from_db(provider.to_string(), model.to_string()).await {
        return Some(cached);
    }
    match fetch().await {
        Ok(limits) => {
            let cached = CachedLimits {
                max_context_tokens: limits.max_context_tokens,
                max_output_tokens: limits.max_output_tokens,
            };
            save_limits_to_db(provider.to_string(), model.to_string(), &cached).await;
            Some(cached)
        },
        // Network/parse failure: don't cache; caller falls back to static.
        Err(_) => None,
    }
}

/// Extract a model's real per-response output ceiling from a provider's 400
/// rejection body. Fires ONLY on unambiguous output-cap wordings:
///
/// - Ollama Cloud / MiniMax: `max_tokens (521276) exceeds model's maximum
///   output tokens (131072) for model …`
/// - OpenAI-compat: `max_tokens is too large: … This model supports at most
///   16384 completion tokens …`
///
/// Anything else — in particular context-limit wordings ("prompt is too
/// long", "maximum context length") — returns `None`: learning a window as
/// an output cap would poison the cache, and a missed match just means
/// today's behavior (the error surfaces). Values outside a sanity range are
/// rejected as parser noise.
pub(crate) fn parse_output_cap_message(body: &str) -> Option<usize> {
    let cap = if let Some(rest) = text_after(body, "exceeds model's maximum output tokens") {
        leading_integer(rest)
    } else if body.contains("max_tokens is too large") {
        text_after(body, "supports at most").and_then(leading_integer)
    } else {
        None
    }?;
    (1_024..10_000_000).contains(&cap).then_some(cap)
}

/// The slice of `haystack` after the first occurrence of `marker`.
fn text_after<'a>(haystack: &'a str, marker: &str) -> Option<&'a str> {
    haystack.find(marker).map(|i| &haystack[i + marker.len()..])
}

/// The first integer in `s`, required to start within a few characters —
/// both documented wordings put the number right after the marker (`" ("` /
/// `" "`), and a distant number would belong to something else.
fn leading_integer(s: &str) -> Option<usize> {
    let start = s.find(|c: char| c.is_ascii_digit()).filter(|&i| i <= 8)?;
    s[start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .ok()
}

/// Decide the output cap for a one-shot retry after learning `learned` from
/// a 400. AUTO (`requested == 0`, including "field omitted") retries at the
/// learned cap; an explicit ask above the cap retries clamped to it; an ask
/// already within the cap returns `None` — the 400 was about something else,
/// so retrying the same request would loop.
pub(crate) fn retry_cap(requested: usize, learned: usize) -> Option<usize> {
    (requested == 0 || requested > learned).then_some(learned)
}

/// `parse_output_cap_message` gated to actual HTTP 400s — the only status
/// where the body names a rejected parameter rather than a transient fault.
pub(crate) fn output_cap_from_error(err: &ModelError) -> Option<usize> {
    match err {
        ModelError::Backend(crate::models::BackendError::HttpError {
            status: 400,
            message,
        }) => parse_output_cap_message(message),
        _ => None,
    }
}

/// Persist an output cap learned from a provider's 400 rejection (the error
/// body names the model's real ceiling). Merges into any existing cached
/// row — reading it raw, ignoring the TTL, since a stale window is still
/// better than dropping it — and upserts as "probed". Best-effort.
pub(crate) async fn learn_output_cap(provider: String, model: String, cap: usize) {
    let _ = tokio::task::spawn_blocking(move || -> Option<()> {
        let store = RuntimeStore::open_default().ok()?;
        let existing = store
            .provider_probes()
            .get(&provider, &model, LIMITS_PROBE_KEY)
            .ok()
            .flatten()
            .and_then(|rec| serde_json::from_str::<CachedLimits>(&rec.capability_value).ok());
        let merged = CachedLimits {
            max_context_tokens: existing.and_then(|l| l.max_context_tokens),
            max_output_tokens: Some(cap),
        };
        let value = serde_json::to_string(&merged).ok()?;
        store
            .provider_probes()
            .upsert(NewProviderProbe {
                provider,
                model_id: model,
                capability_key: LIMITS_PROBE_KEY.into(),
                capability_value: value,
                confidence: "probed".into(),
                error: None,
            })
            .ok()?;
        Some(())
    })
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    // The incident wording (Ollama Cloud, minimax-m3), raw and JSON-wrapped.
    const MINIMAX_RAW: &str =
        "max_tokens (521276) exceeds model's maximum output tokens (131072) for model minimax-m3";
    const MINIMAX_JSON: &str = r#"{"error":"max_tokens (521276) exceeds model's maximum output tokens (131072) for model minimax-m3 (ref: a05c9ffb-168f)"}"#;
    const OPENAI_STYLE: &str = r#"{"error":{"message":"max_tokens is too large: 200000. This model supports at most 16384 completion tokens, whereas you provided 200000.","type":"invalid_request_error"}}"#;

    #[test]
    fn parse_output_cap_matches_documented_wordings() {
        assert_eq!(parse_output_cap_message(MINIMAX_RAW), Some(131_072));
        assert_eq!(parse_output_cap_message(MINIMAX_JSON), Some(131_072));
        assert_eq!(parse_output_cap_message(OPENAI_STYLE), Some(16_384));
    }

    #[test]
    fn parse_output_cap_never_matches_context_limit_wordings() {
        // Learning a context window as an output cap would poison the cache —
        // these must all be None even though they mention token limits.
        for body in [
            "prompt is too long: 210000 tokens > 200000 maximum",
            "This model's maximum context length is 128000 tokens",
            "input length and max_tokens exceed context limit: 190000 + 20000 > 200000",
            "the request exceeds the maximum context window of 131072 tokens",
            "rate limit exceeded, try again in 20s",
            "",
        ] {
            assert_eq!(parse_output_cap_message(body), None, "matched: {body}");
        }
    }

    #[test]
    fn parse_output_cap_rejects_nonsense_values() {
        // Sub-1024 and absurd values are parser noise, not real ceilings.
        assert_eq!(
            parse_output_cap_message("exceeds model's maximum output tokens (512)"),
            None
        );
        assert_eq!(
            parse_output_cap_message("exceeds model's maximum output tokens (99999999999)"),
            None
        );
        // A number too far from the marker belongs to something else.
        assert_eq!(
            parse_output_cap_message(
                "exceeds model's maximum output tokens for this deployment tier which is 131072"
            ),
            None
        );
    }

    #[test]
    fn retry_cap_triple() {
        // AUTO (0 / omitted) → retry at the learned cap.
        assert_eq!(retry_cap(0, 131_072), Some(131_072));
        // Explicit ask above the cap → clamp.
        assert_eq!(retry_cap(521_276, 131_072), Some(131_072));
        // Ask already within the cap → the 400 was about something else.
        assert_eq!(retry_cap(4_096, 131_072), None);
    }

    #[test]
    fn output_cap_from_error_gates_on_http_400() {
        let err_400 = ModelError::Backend(crate::models::BackendError::HttpError {
            status: 400,
            message: MINIMAX_JSON.to_string(),
        });
        assert_eq!(output_cap_from_error(&err_400), Some(131_072));
        // Same body on a 500 is a transient fault, not a learned limit.
        let err_500 = ModelError::Backend(crate::models::BackendError::HttpError {
            status: 500,
            message: MINIMAX_JSON.to_string(),
        });
        assert_eq!(output_cap_from_error(&err_500), None);
        assert_eq!(output_cap_from_error(&ModelError::Cancelled), None);
    }
}
