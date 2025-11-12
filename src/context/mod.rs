// Gateway module for context - follows the Train Station Pattern
// All external access must go through this gateway

// Private submodules - not directly accessible from outside
mod file_collector;
mod loader;
mod manager;
mod project_detector;
mod token_counter;

// Public re-exports - the ONLY way to access context functionality
pub use loader::{ContextLoader, LoaderConfig};
pub use manager::ContextManager;
