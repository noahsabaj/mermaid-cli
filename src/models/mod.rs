// Gateway module for models - follows the Train Station Pattern
// All external access must go through this gateway

// Private submodules - not directly accessible from outside
mod backend;
mod backend_model_adapter;
mod backends;
mod factory;
mod lazy_context;
mod ollama_direct;
mod registry;
mod traits;
mod types;
mod unified;
mod unified_backend;

// Public re-exports - the ONLY way to access model functionality
pub use backends::{OllamaBackend, VLLMBackend};
pub use factory::ModelFactory;
pub use lazy_context::{get_priority_files, LazyProjectContext};
pub use registry::BackendRegistry;
pub use traits::Model;
pub use types::{
    ChatMessage, MessageRole, ModelCapabilities, ModelConfig, ModelResponse, ProjectContext,
    StreamCallback, TokenUsage,
};
pub use unified::create_from_string;
pub use unified_backend::UnifiedBackend;
