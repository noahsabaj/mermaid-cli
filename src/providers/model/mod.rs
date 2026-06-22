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
use crate::models::{ModelError, Result, TokenUsage};

use super::capabilities::Capabilities;
use super::ctx::{FinalResponse, StreamContext, StreamEvent};

/// Provider-facing interface. A `ModelProvider` impl owns whatever
/// HTTP client / state it needs and exposes `chat()` — that's the
/// whole surface.
#[async_trait]
pub trait ModelProvider: Send + Sync {
    /// Capabilities the provider advertises. The reducer reads this
    /// when building the outgoing `ChatRequest` (e.g. whether to
    /// attach reasoning controls).
    fn capabilities(&self) -> &Capabilities;

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
                StreamEvent::Reasoning(_)
                | StreamEvent::ToolCall(_)
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
