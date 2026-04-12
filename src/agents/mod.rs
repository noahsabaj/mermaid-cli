// Gateway module for agents - follows the Train Station Pattern
// All external access must go through this gateway

// Private submodules - not directly accessible from outside
mod action_executor;
mod computer_use;
mod executor;
mod filesystem;
mod subagent;
mod types;
mod web_search;

// Public re-exports - the ONLY way to access agent functionality
pub use action_executor::{
    describe_action, execute_action, get_mcp_manager, mark_mcp_init_complete,
    mark_mcp_init_started, set_mcp_manager,
};
pub use filesystem::{is_binary_file, read_binary_file, read_file};
pub use subagent::{
    SubagentProgress, SubagentResult, SubagentStatus, collect_subagent_results,
    format_subagent_tool_result, spawn_subagents,
};
pub use types::{ActionDetails, ActionDisplay, ActionResult, AgentAction};
pub use web_search::{SearchResult, WebFetchResult, WebSearchClient};
