// Gateway module for models
// All external access must go through this gateway

// Core modules
mod adapters; // Provider adapters (Ollama, OpenAI-compatible)
mod backend; // ModelFactory (single factory)
mod capabilities; // Per-model capability flags
mod config; // Unified configuration
mod error; // Structured error types
mod providers; // OpenAI-compatible provider profiles + registry
mod reasoning; // ReasoningLevel, ReasoningCapability, nearest_effort
mod retry; // Transient-failure retry policy for provider HTTP calls
mod stream; // Typed StreamEvent enum (replaces text-only callback)
pub mod tool_call; // Tool call parsing (native function calling)
pub mod tools; // Tool definitions
mod traits; // Model trait (public API)
mod types; // Core types (ChatMessage, etc)

// Public re-exports - the ONLY way to access model functionality
pub use backend::ModelFactory;
pub use capabilities::ModelCapabilities;
pub use config::{BackendConfig, ModelConfig, OllamaOptions};
pub use error::{BackendError, ConfigError, ErrorCategory, ModelError, Result, UserFacingError};
pub use providers::{
    CompatStyle, ProviderProfile, REGISTRY as PROVIDER_REGISTRY, ReasoningExtraction,
    ReasoningStrategy, lookup_provider,
};
pub use reasoning::{ReasoningCapability, ReasoningChunk, ReasoningLevel, nearest_effort};
pub use retry::retry_transient_http;
pub use stream::{StreamCallback, StreamEvent};
pub use tool_call::{FunctionCall, ToolCall};
pub use tools::{Tool, ToolFunction, ToolRegistry};
pub use traits::Model;
pub use types::{ChatMessage, MessageRole, ModelResponse, TokenUsage};
