// Gateway module for models - follows the Train Station Pattern
// All external access must go through this gateway

// Core new architecture - private
mod adapters; // Provider adapters (Ollama, vLLM)
mod backend;  // Backend trait and factory
mod config;   // Unified configuration
mod error;    // Structured error types
mod factory;  // Model factory (public API)
mod model;    // UnifiedModel implementation
mod router;   // Smart backend router
mod traits;   // Model trait (public API)
mod types;    // Core types (ChatMessage, etc)

// Keep these for now (will migrate incrementally)
mod lazy_context;

// Public re-exports - the ONLY way to access model functionality
pub use config::{BackendConfig, ModelConfig};
pub use error::{BackendError, ConfigError, ModelError, Result};
pub use factory::ModelFactory;
pub use lazy_context::{get_priority_files, LazyProjectContext};
pub use model::{create_model, create_model_default, UnifiedModel};
pub use traits::Model;
pub use types::{
    ChatMessage, MessageRole, ModelResponse, ProjectContext,
    StreamCallback, TokenUsage,
};

// Re-export router for advanced use cases
pub use router::BackendRouter;
