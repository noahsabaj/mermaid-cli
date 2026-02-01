// Gateway module for models
// All external access must go through this gateway

// Core modules
mod adapters;  // Provider adapters (Ollama, OpenAI-compatible)
mod backend;   // Internal ModelFactory
mod config;    // Unified configuration
mod error;     // Structured error types
mod factory;   // Public ModelFactory API
mod traits;    // Model trait (public API)
mod types;     // Core types (ChatMessage, etc)
pub mod tool_call; // Tool call parsing (native function calling)
pub mod tools;     // Tool definitions

// Keep these for now (will migrate incrementally)
mod lazy_context;

// Public re-exports - the ONLY way to access model functionality
pub use config::{BackendConfig, ModelConfig};
pub use error::{BackendError, ConfigError, ErrorCategory, ModelError, Result, UserFacingError};
pub use factory::ModelFactory;
pub use lazy_context::{get_priority_files, LazyProjectContext};
pub use traits::{Model, ModelCapabilities};
pub use tool_call::{parse_tool_calls, group_parallel_reads, ToolCall, FunctionCall};
pub use tools::{Tool, ToolFunction, ToolRegistry};
pub use types::{
    ChatMessage, MessageRole, ModelResponse, ProjectContext,
    StreamCallback, TokenUsage,
};
