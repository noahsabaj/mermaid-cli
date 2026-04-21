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

pub use anthropic::AnthropicProvider;
pub use gemini::GeminiProvider;
pub use ollama::OllamaProvider;
pub use openai_compat::OpenAICompatProvider;
