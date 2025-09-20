// Gateway module for context - follows the Train Station Pattern
// All external access must go through this gateway

// Private submodules - not directly accessible from outside
mod analyzer;
mod loader;

// Public re-exports - the ONLY way to access context functionality
pub use loader::{ContextLoader, LoaderConfig};