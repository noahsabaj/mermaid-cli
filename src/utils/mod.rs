// Gateway module for utils - follows the Train Station Pattern
// All external access must go through this gateway

// Private submodules - not directly accessible from outside
mod errors;
mod syntax;

// Public re-exports - the ONLY way to access utils functionality
pub use errors::MermaidError;