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
use crate::models::adapters::ollama_sizing::NumCtxSource;
use crate::models::{ModelError, Result, TokenUsage};

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
