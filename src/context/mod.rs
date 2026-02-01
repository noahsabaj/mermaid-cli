// Gateway module for context - follows the Train Station Pattern
// All external access must go through this gateway

// Private submodules - not directly accessible from outside
mod context;
mod file_collector;
mod token_counter;

// Public re-exports - the ONLY way to access context functionality
pub use context::{Context, ContextConfig};
