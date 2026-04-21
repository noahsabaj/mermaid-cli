//! Pure-function reducer: `fn update(State, Msg) -> (State, Vec<Cmd>)`.
//!
//! Heart of the MVU architecture. Everything here is synchronous,
//! does no I/O, and is testable without tokio or a terminal. The
//! effect runner (`crate::effect`) takes the `Cmd` values the reducer
//! emits and performs the actual work; results come back as `Msg`
//! events that feed into another `update` call.
//!
//! The split is load-bearing: stale events racing with cancellation,
//! lost tool results, and two event loops competing for input are all
//! impossible to express against these types.

pub mod action;
pub mod cmd;
pub mod ids;
pub mod msg;
pub mod reducer;
pub mod slash_commands;
pub mod state;
pub mod transition;

pub use action::{ActionDetails, ActionDisplay, ActionResult};
pub use cmd::{ChatRequest, Cmd, ToolDefinition};
pub use ids::{IdAllocator, ToolCallId, TurnId};
pub use msg::{Key, KeyCode, KeyMods, Msg, MsgKind, Paste, SlashCmd, StartupConfig};
pub use reducer::{build_chat_request, update};
pub use slash_commands::{COMMAND_REGISTRY, SlashCommand, filter_by_prefix};
pub use state::{
    Attachment, Confirmation, ConfirmationTarget, ConversationSummary, GenPhase, IdAllocatorBundle,
    McpServerEntry, McpServerStatus, McpState, McpToolSpec, PendingToolCall, Session, State,
    StatusKind, StatusLine, ToolOutcome, TurnState, UiMode, UiState,
};
pub use transition::{
    action_display_for, commit_assistant_message, fill_outcome, start_executing_tools,
    start_generating, tool_result_messages, try_complete_outcomes,
};
