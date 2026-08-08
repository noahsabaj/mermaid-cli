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

pub mod cmd;
pub mod compaction;
pub mod file_mention;
pub mod image_token;
pub mod msg;
pub mod plan;
pub mod reducer;
pub mod run_event;
pub mod runtime;
pub mod slash_commands;
pub mod state;
pub mod tasks;
pub mod tool_search;
pub mod transition;

pub use cmd::{ChatRequest, Cmd, ToolDefinition};
pub use compaction::{
    CompactionArchive, CompactionBoundary, CompactionPolicy, CompactionRecord, CompactionRequest,
    CompactionResult, CompactionReviewStatus, CompactionTrigger, PreparedCompaction,
    build_replacement_messages, build_summary_request, build_verification_request, combine_usage,
    compaction_receipt, context_exceeds_hard_limit, format_compact_count, normalize_summary,
    prepare_compaction, should_auto_compact, validate_summary_structure,
};
pub use mermaid_model::action::{ActionDetails, ActionDisplay, ActionResult};
pub use mermaid_model::ids::{IdAllocator, ToolCallId, TurnId};
pub use mermaid_model::question::{
    OptionPreview, PendingQuestionSet, Question, QuestionAnswer, QuestionKind, QuestionOption,
    QuestionResolution, QuestionSelection, TextValidate, rank_order, validate_input,
};
pub use mermaid_model::tool_run::{
    ManagedProcess, ManagedProcessStatus, ToolArtifact, ToolMetadata, ToolRunMetadata, ToolStatus,
    WebSearchFailure,
};
pub use msg::{
    ClipboardRead, ContextCmd, Key, KeyCode, KeyMods, Msg, MsgKind, Paste, SlashCmd, StartupConfig,
};
pub use reducer::{build_chat_request, update};
pub use run_event::{RUN_EVENT_PROTOCOL_VERSION, RunEvent};
pub use runtime::{
    ProviderCapabilitySnapshot, RuntimeSignal, RuntimeState, RuntimeTimelineEvent,
    RuntimeTimelineKind,
};
pub use slash_commands::{COMMAND_GROUPS, COMMAND_REGISTRY, SlashCommand, filter_by_prefix};
pub use state::{
    AdvertisedContext, ApprovalChoice, ApprovalKind, Attachment, Confirmation, ConfirmationTarget,
    ContextUsageSnapshot, ConversationSummary, GenPhase, IdAllocatorBundle, LiveToolStatus,
    McpServerEntry, McpServerStatus, McpState, McpToolSpec, ModelChoice, PendingApproval,
    PendingToolCall, PlanState, PluginCommand, PromptTokenBreakdown, QueuedMessage,
    RewindCandidate, Session, State, StatusKind, TokenUsageTotals, ToolOutcome, TurnState, UiMode,
    UiState, estimate_context_usage_for_request, estimate_tool_schema_tokens,
};
pub use tasks::{
    ApplyReport, EvidenceEntry, Stamp, TaskEdit, TaskItem, TaskOrigin, TaskSpec, TaskStatus,
    TaskStore, UserTaskEdit, advisory_notes,
};
pub use transition::{
    action_display_for, commit_assistant_message, display_info_for, fill_outcome,
    start_executing_tools, start_generating, tool_result_messages, try_complete_outcomes,
};
