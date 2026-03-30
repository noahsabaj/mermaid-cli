// Gateway module for models
// All external access must go through this gateway

// Core modules
mod adapters; // Provider adapters (Ollama, OpenAI-compatible)
mod backend; // ModelFactory (single factory)
mod config; // Unified configuration
mod error; // Structured error types
pub mod tool_call; // Tool call parsing (native function calling)
pub mod tools;
mod traits; // Model trait (public API)
mod types; // Core types (ChatMessage, etc) // Tool definitions

// Public re-exports - the ONLY way to access model functionality
pub use backend::ModelFactory;
pub use config::{BackendConfig, ModelConfig, OllamaOptions};
pub use error::{BackendError, ConfigError, ErrorCategory, ModelError, Result, UserFacingError};
pub use tool_call::{FunctionCall, ToolCall};
pub use tools::{Tool, ToolFunction, ToolRegistry};
pub use traits::Model;
pub use types::{ChatMessage, MessageRole, ModelResponse, StreamCallback, TokenUsage};
