//! Pure-function reducer: `fn update(State, Msg) -> (State, Vec<Cmd>)`.
//!
//! This module is the heart of the v0.7 architecture. Everything here
//! is synchronous, does no I/O, and is testable without tokio or a
//! terminal. The effect runner (`crate::effect`) takes the `Cmd`
//! values the reducer emits and performs the actual work; results
//! come back as `Msg` events that feed into another `update` call.
//!
//! The split is load-bearing: bugs that the old architecture made
//! possible (stale events racing with cancellation, lost tool results,
//! two event loops competing for input) are impossible to express
//! against these types. See `docs/architecture.md` (added in commit
//! 11) for the full rationale.

pub mod cmd;
pub mod ids;
pub mod msg;
pub mod reducer;
pub mod state;
pub mod transition;

pub use cmd::{ChatRequest, Cmd, ToolDefinition};
pub use ids::{IdAllocator, SubagentId, ToolCallId, TurnId};
pub use msg::{Key, KeyCode, KeyMods, Msg, MsgKind, Paste, SlashCmd, StartupConfig};
pub use reducer::{build_chat_request, update};
pub use state::{
    Attachment, Confirmation, ConfirmationTarget, GenPhase, IdAllocatorBundle, McpServerEntry,
    McpServerStatus, McpState, McpToolSpec, PendingToolCall, Session, State, StatusKind,
    StatusLine, SubagentProgress, SubagentSpec, SubagentStatus, ToolOutcome, TurnState, UiMode,
    UiState,
};
pub use transition::{
    action_display_for, commit_assistant_message, fill_outcome, start_executing_tools,
    start_generating, tool_result_messages, try_complete_outcomes,
};
