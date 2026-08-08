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

pub mod checklist;
pub mod cmd;
pub mod compaction;
pub mod config;
pub mod conversation;
pub mod file_mention;
pub mod image_token;
pub mod msg;
pub mod plan;
pub mod progress;
pub mod reducer;
pub mod run_event;
pub mod runtime;
pub mod slash_commands;
pub mod state;
pub mod tool_search;
pub mod transition;

pub use checklist::{
    ChecklistEdit, ChecklistItem, ChecklistOrigin, ChecklistSpec, ChecklistStatus, ChecklistStore,
    EvidenceEntry, Stamp, UserChecklistEdit, advisory_notes,
};
pub use cmd::{ChatRequest, Cmd, ToolDefinition};
pub use compaction::{
    CompactionArchive, CompactionBoundary, CompactionEvent, CompactionPolicy, CompactionRequest,
    CompactionResult, CompactionReviewStatus, CompactionTrigger, PreparedCompaction,
    build_replacement_messages, build_summary_request, build_verification_request, combine_usage,
    compaction_receipt, context_exceeds_hard_limit, format_compact_count, normalize_summary,
    prepare_compaction, should_auto_compact, validate_summary_structure,
};
pub use config::{
    AgentTypeConfig, AgentsConfig, CompactionConfig, Config, ConfigLayer, ExecConfig, FetchBackend,
    FilesystemPolicy, McpServerConfig, MemoryConfig, NetworkPolicy, PlanConfig, PlanPermLevel,
    PlanPermissions, PlanPostApprove, SafetyConfig, SearchBackend, SessionFlags, ThemeChoice,
    TransportKind, UiConfig, UserProviderConfig, WebConfig,
};
pub use conversation::ConversationHistory;
pub use mermaid_model::action::{ActionDetails, ActionDisplay, ActionResult};
pub use mermaid_model::ids::{ToolCallId, TurnId};
pub use mermaid_model::question::{
    PendingQuestionSet, Question, QuestionAnswer, QuestionKind, QuestionOption, QuestionResolution,
    rank_order, validate_input,
};
pub use mermaid_model::tool_run::{
    ManagedProcess, ToolArtifact, ToolMetadata, ToolRunMetadata, ToolStatus, WebSearchFailure,
};
pub use msg::{
    ClipboardRead, ContextCmd, Key, KeyCode, KeyMods, Msg, MsgKind, Paste, SlashCmd, StartupConfig,
};
pub use progress::{ProgressEvent, SubagentPhase};
pub use reducer::{build_chat_request, update};
pub use run_event::{RUN_EVENT_PROTOCOL_VERSION, RunEvent};
pub use runtime::{
    ProviderCapabilitySnapshot, RuntimeSignal, RuntimeState, RuntimeTimelineEvent,
    RuntimeTimelineKind,
};
pub use slash_commands::{COMMAND_GROUPS, COMMAND_REGISTRY, SlashCommand, filter_by_prefix};
pub use state::{
    AdvertisedContext, ApprovalChoice, ApprovalKind, Attachment, Confirmation, ConfirmationTarget,
    ContextUsageSnapshot, ConversationSummary, GenPhase, LiveToolStatus, McpServerEntry,
    McpServerStatus, McpState, McpToolSpec, ModelChoice, PendingApproval, PendingToolCall,
    PlanState, PluginCommand, PromptTokenBreakdown, QueuedMessage, RewindCandidate, Session, State,
    StatusKind, TokenUsageTotals, ToolOutcome, TurnState, UiMode, UiState,
    estimate_context_usage_for_request, estimate_tool_schema_tokens,
};
pub use transition::{
    action_display_for, commit_assistant_message, display_info_for, fill_outcome,
    start_executing_tools, start_generating, tool_result_messages, try_complete_outcomes,
};
