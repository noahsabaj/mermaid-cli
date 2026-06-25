// Gateway module for models
// All external access must go through this gateway

pub(crate) mod adapters; // Provider adapters (Ollama, OpenAI-compatible) — wrapped by providers/model/*
mod capabilities; // Per-model capability flags
pub(crate) mod config; // Unified configuration
mod error; // Structured error types
mod providers; // OpenAI-compatible provider profiles + registry
mod reasoning; // ReasoningLevel, ReasoningCapability, nearest_effort
mod stream; // Typed StreamEvent enum
pub mod tool_call; // Tool call parsing (native function calling)

mod traits; // Model trait (public API)
mod types; // Core types (ChatMessage, etc)

// Public re-exports — the ONLY way to access model functionality
pub use capabilities::ModelCapabilities;
pub use config::{BackendConfig, ModelConfig, OllamaOptions};
pub use error::{BackendError, ConfigError, ErrorCategory, ModelError, Result, UserFacingError};
pub use providers::{
    CompatStyle, MaxTokensParam, ProviderProfile, REGISTRY as PROVIDER_REGISTRY,
    ReasoningExtraction, ReasoningStrategy, lookup_provider,
};
pub use reasoning::{ReasoningCapability, ReasoningChunk, ReasoningLevel, nearest_effort};
pub use stream::{StreamCallback, StreamEvent};
pub use tool_call::{FunctionCall, ToolCall};

pub use traits::Model;
pub use types::{
    ChatMessage, ChatMessageKind, FinishReason, MessageRole, ModelResponse, TokenUsage,
    TokenUsageSource,
};
