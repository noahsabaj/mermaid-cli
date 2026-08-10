//! The pure Model-View-Update core, and nothing above it.
//!
//! `fn update(State, Msg) -> (State, Vec<Cmd>)`: synchronous, no I/O, no wall
//! clock (it reads `state.now`, injected, so `--replay` is deterministic).
//! Effects are DATA — the `Cmd` values this crate returns are executed by the
//! impure shell in `mermaid-cli`.
//!
//! It is a crate because the property was asserted and not enforced. A guard
//! that greps `src/domain` for `std::fs` and `tokio::` cannot see
//! `use crate::app::Config`, so the "pure" core had accumulated 34 upward
//! edges — into `app`, `session`, `providers` and `render` — and two of them
//! were cycles:
//!
//!   * `app::CompactionConfig::policy()` returned a `domain::CompactionPolicy`
//!     while `domain::State` held an `app::Config`. Neither module could be
//!     compiled without the other.
//!   * `app::stamp_session_provenance` took `&mut domain::State`, against
//!     `state.rs`'s own rule that only `update()` may hold one — and because
//!     that stamp was not a recorded `Msg`, `--replay` overwrote a recording's
//!     git branch with the replaying machine's.
//!
//! Both are gone, and now unrepresentable: `use crate::app::Config` in here is
//! an `unresolved import`, not a review comment. Same play as `mermaid-model`
//! (commit 7c2f2ad); see that crate's docs for the five edges it fixed.
//!
//! What is absent from `Cargo.toml` is part of the enforcement: no `tokio`, no
//! `reqwest`, no `rusqlite`, no `crossterm`, no `clap`. A reducer that wants to
//! await something cannot, because the runtime is not on the dependency list.

pub mod action_display;
pub mod checklist;
pub mod cmd;
pub mod compaction;
pub mod config;
pub mod context;
pub mod conversation;
pub mod file_mention;
pub mod image_token;
pub mod msg;
pub mod plan;
pub mod progress;
pub mod prompts;
pub mod query;
pub mod reducer;
pub mod reports;
pub mod request;
pub mod run_event;
pub mod runtime;
pub mod slash_commands;
pub mod state;
pub mod tool_search;
pub mod transition;

pub use action_display::{action_display_for, display_info_for, display_info_for_shell};
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
pub use context::{
    InstructionSource, LoadedInstructions, LoadedMemory, LoadedSkills, MemoryEntry, MemoryScope,
    SessionProvenance, SkillEntry, SkillSource,
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
pub use query::{Query, QueryResult};
pub use reducer::update;
pub use request::build_chat_request;
pub use run_event::{RUN_EVENT_PROTOCOL_VERSION, RunEvent};
pub use runtime::{
    ProviderCapabilitySnapshot, RuntimeSignal, RuntimeState, RuntimeTimelineEvent,
    RuntimeTimelineKind,
};
pub use slash_commands::{
    COMMAND_GROUPS, COMMAND_REGISTRY, SlashCommand, filter_by_prefix, parse_slash_command,
};
pub use state::{
    AdvertisedContext, ApprovalChoice, ApprovalKind, Attachment, Confirmation, ConfirmationTarget,
    ContextUsageSnapshot, ConversationSummary, GenPhase, LiveToolStatus, McpServerEntry,
    McpServerStatus, McpState, McpToolSpec, ModelChoice, PendingApproval, PendingToolCall,
    PlanState, PluginCommand, PromptTokenBreakdown, QueuedMessage, RewindCandidate, Session, State,
    StatusKind, TokenUsageTotals, ToolOutcome, TurnState, UiMode, UiState,
    estimate_context_usage_for_request, estimate_tool_schema_tokens,
};
pub use transition::{
    commit_assistant_message, fill_outcome, start_executing_tools, start_generating,
    tool_result_messages, try_complete_outcomes,
};
