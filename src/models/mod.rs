// Gateway module for models - follows the Train Station Pattern
// All external access must go through this gateway

// Private submodules - not directly accessible from outside
mod factory;
mod lazy_context;
mod traits;
mod types;
mod unified;

// Public re-exports - the ONLY way to access model functionality
pub use factory::ModelFactory;
pub use lazy_context::{LazyProjectContext, get_priority_files};
pub use traits::Model;
pub use types::{
    ChatMessage, MessageRole, ModelCapabilities, ModelConfig, ModelResponse, ProjectContext,
    StreamCallback, TokenUsage,
};
pub use unified::create_from_string;