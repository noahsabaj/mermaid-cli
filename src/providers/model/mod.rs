//! Model adapters wrapped as `ModelProvider` implementations.
//!
//! In C3 only Ollama lands here (proof of pattern). C4 moves
//! OpenAI-compat, Anthropic, and Gemini over. The v0.6 adapter
//! tree at `src/models/adapters/*.rs` stays in parallel during
//! migration so the old runtime keeps compiling; both trees
//! delete together in C10 when the new main loop goes live.

pub mod anthropic;
pub mod gemini;
pub mod ollama;
pub mod openai_compat;

use async_trait::async_trait;

use crate::domain::ChatRequest;
use crate::models::Result;

use super::capabilities::Capabilities;
use super::ctx::{FinalResponse, StreamContext};

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
    /// `ctx.sink`; the returned `FinalResponse` is consumed by the
    /// effect runner for logging + subagent bookkeeping.
    ///
    /// Cancellation: the provider MUST select! on `ctx.token.
    /// cancelled()` inside any await that could block for more than
    /// a few hundred ms. This is the contract that replaces the old
    /// `check_interrupt` polling pattern.
    async fn chat(&self, request: ChatRequest, ctx: StreamContext) -> Result<FinalResponse>;
}

pub use anthropic::AnthropicProvider;
pub use gemini::GeminiProvider;
pub use ollama::OllamaProvider;
pub use openai_compat::OpenAICompatProvider;
