// Gateway module for agents - follows the Train Station Pattern
// All external access must go through this gateway

// Private submodules - not directly accessible from outside
mod action_executor;
mod executor;
mod filesystem;
mod git;
mod mode_aware_executor;
mod parser;
mod types;

// Public re-exports - the ONLY way to access agent functionality
pub use action_executor::execute_action;
pub use mode_aware_executor::ModeAwareExecutor;
pub use parser::parse_actions;
pub use types::{ActionResult, AgentAction};