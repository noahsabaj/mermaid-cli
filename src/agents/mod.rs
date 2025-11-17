// Gateway module for agents - follows the Train Station Pattern
// All external access must go through this gateway

// Private submodules - not directly accessible from outside
mod action_executor;
mod executor;
mod filesystem;
mod git;
mod mode_aware_executor;
mod plan;
mod types;
mod web_search;

// Public re-exports - the ONLY way to access agent functionality
pub use action_executor::execute_action;
pub use filesystem::{is_binary_file, read_binary_file, read_file};
pub use mode_aware_executor::ModeAwareExecutor;
pub use plan::{ActionStatus, Plan, PlanStats, PlannedAction};
pub use types::{ActionDisplay, ActionResult, AgentAction};
pub use web_search::{SearchResult, WebSearchClient};
