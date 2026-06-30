//! Every input to the reducer.
//!
//! `Msg` is an exhaustive sum over three categories:
//!
//!   1. **User intent** — key presses, pastes, slash commands, submit,
//!      cancel, quit. Originates from `app::event_source`.
//!   2. **Effect results** — stream chunks, tool outcomes, MCP
//!      lifecycle, save/load completion. Originates from
//!      `effect::EffectRunner` when a spawned task finishes a unit of
//!      work.
//!   3. **Housekeeping** — `Tick` (timer-driven redraw), `StatusDismiss`,
//!      `InstructionsChanged` (mtime watcher).
//!
//! Every effect-result variant carries a `TurnId`. The reducer's first
//! gate on any such message is `if state.turn.accepts(msg.turn_id())`
//! — messages for a cancelled / superseded turn are dropped without
//! state change. This is the architectural guarantee that stale
//! streaming events can never corrupt the current turn.

use std::path::PathBuf;

use crate::app::McpServerConfig;
use crate::app::instructions::LoadedInstructions;
use crate::models::tool_call::ToolCall as ModelToolCall;
use crate::models::{FinishReason, ReasoningChunk, ReasoningLevel, TokenUsage, UserFacingError};
use crate::runtime::{
    ApprovalRecord, CheckpointRecord, PluginInstallRecord, ProcessRecord, SafetyMode, TaskRecord,
    TaskTimelineEvent,
};

use super::ids::{ToolCallId, TurnId};
use super::runtime::RuntimeSignal;
use super::state::ContextUsageSnapshot;
use super::state::StatusKind;
use super::state::{ApprovalKind, ConversationSummary, McpToolSpec, ToolOutcome};
use super::{CompactionResult, CompactionTrigger};

/// Single reducer input. Non-exhaustive is intentional: adding a new
/// variant is a deliberate act that forces every reducer arm to
/// consider it at compile time (the reducer's match is NOT
/// `_ =>` — see `reducer.rs`).
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum Msg {
    // ── User intent ─────────────────────────────────────────────────
    /// Raw key event from crossterm, after the event source has
    /// stripped mouse/resize/paste.
    Key(Key),
    /// A full paste (text OR image) from the terminal.
    Paste(Paste),
    /// User hit Enter on a non-empty input. The event source has
    /// already stripped the slash-command routing.
    SubmitPrompt {
        text: String,
        /// Attachment IDs the reducer should consume from state.
        attachment_ids: Vec<u64>,
    },
    /// User ran a slash command (post-routing from `app::event_source`).
    Slash(SlashCmd),
    /// Esc or another explicit cancellation source during an active turn.
    CancelTurn,
    /// Confirmation modal answer.
    ConfirmAccepted,
    ConfirmDeclined,
    /// User wants to exit cleanly (Ctrl+D with empty input, or `/quit`).
    Quit,
    /// External process lifecycle signal. In raw-mode TUI sessions a
    /// typed Ctrl+C still arrives as `Msg::Key`; this variant covers
    /// OS-level SIGINT/SIGTERM/SIGHUP delivered from outside.
    RuntimeSignal(RuntimeSignal),

    // ── Streaming (from effect::model) ──────────────────────────────
    /// Chunk of assistant text. Append to `partial_text`.
    StreamText {
        turn: TurnId,
        chunk: String,
    },
    /// Chunk of reasoning / thinking content.
    StreamReasoning {
        turn: TurnId,
        chunk: ReasoningChunk,
    },
    /// Model emitted a tool call. Append to the outgoing call list;
    /// actual execution dispatches on `StreamDone`.
    StreamToolCall {
        turn: TurnId,
        call: ModelToolCall,
    },
    /// Effect runner estimated the fully-enriched request context
    /// after built-in and MCP tool schemas were attached.
    ContextUsageEstimated {
        turn: TurnId,
        snapshot: ContextUsageSnapshot,
    },
    /// Effect runner resolved the provider's context window. For Ollama,
    /// `model_max` is the probed architectural window and `effective` is the
    /// auto-fitted/overridden `num_ctx`. Model-level metadata (not turn-scoped);
    /// `model_id` is carried so a probe that lands after a `/model` switch can
    /// be dropped instead of overwriting the new model's window. Drives the
    /// `/context` display + quick-fix.
    ProviderContextResolved {
        model_id: String,
        model_max: Option<usize>,
        effective: Option<usize>,
        source: Option<crate::models::adapters::ollama_sizing::NumCtxSource>,
    },
    /// Effect runner verified the loaded model's memory placement after a turn
    /// (Ollama `/api/ps`). `size_vram_bytes < total_bytes` ⇒ the model spilled
    /// to CPU/RAM (slow). Model-level metadata (not turn-scoped); `model_id` is
    /// carried so a probe that lands after a `/model` switch can be dropped.
    /// Drives the offload warning + `/context` placement line.
    OllamaPlacementResolved {
        model_id: String,
        size_vram_bytes: u64,
        total_bytes: u64,
        /// Auto-converge target: largest `num_ctx` that would fit when the model
        /// spilled, or `None` if it fits / can't be helped by shrinking.
        suggested_num_ctx: Option<u32>,
    },
    /// The effect runner's estimate of the built-in tool-schema token cost
    /// it appends to every request during dispatch. Not turn-scoped — the
    /// reducer stores it on `runtime` so `/context` can fold it into its
    /// MCP-only estimate and agree with what dispatch actually decides.
    BuiltinToolSchemaTokens(usize),
    /// Context compaction completed and produced a replacement
    /// model-visible history.
    CompactionFinished {
        turn: TurnId,
        result: CompactionResult,
    },
    /// Context compaction failed or no-oped. Manual failures end the
    /// compaction turn; auto failures may leave generation running.
    CompactionFailed {
        turn: TurnId,
        trigger: CompactionTrigger,
        message: String,
        kind: StatusKind,
    },
    /// Stream complete. Carries final token count (0 if unknown) and,
    /// for Anthropic, the thinking signature that must round-trip on
    /// the next request.
    StreamDone {
        turn: TurnId,
        usage: Option<TokenUsage>,
        thinking_signature: Option<String>,
        /// Why the model stopped (truncation / content block / normal), when
        /// the provider reported it. Drives the truncation status note.
        stop_reason: Option<FinishReason>,
    },
    /// Upstream returned a recoverable or terminal error. Reducer
    /// commits an error line and returns to `Idle` (or surfaces a
    /// retry affordance, if `recoverable`).
    UpstreamError {
        turn: TurnId,
        error: UserFacingError,
    },
    /// Terminal event for a cancelled turn. Emitted by the effect
    /// runner's `drop_scope` once every child task in the turn's
    /// `TurnScope` has unwound. Reducer transitions
    /// `Cancelling(id) → Idle` when it arrives.
    ///
    /// Without this, the reducer relies on the (wrong) side-channel of
    /// `UpstreamError` arriving from a cancelled provider call to exit
    /// `Cancelling`. If the provider task is aborted before it can
    /// emit an error, the state would stick in `Cancelling` forever.
    TurnCancelled(TurnId),

    // ── Tools (from effect::tool) ───────────────────────────────────
    /// Tool was picked up by the executor — useful for "spinner
    /// started" UI transitions.
    ToolStarted {
        turn: TurnId,
        call_id: ToolCallId,
    },
    /// Mid-flight progress (streaming subprocess output, byte-count
    /// updates, multimodal artifacts, nested subagent activity).
    /// Reducer pattern-matches the variant and routes accordingly:
    /// text variants update the status line; `Artifact` with an
    /// `image/*` mime attaches to the in-flight assistant message;
    /// `Subagent*` variants render as indented status.
    ToolProgress {
        turn: TurnId,
        call_id: ToolCallId,
        event: crate::providers::ProgressEvent,
    },
    /// Tool finished (one of Finished / Error / Cancelled).
    ToolFinished {
        turn: TurnId,
        call_id: ToolCallId,
        outcome: ToolOutcome,
    },
    /// A gated tool is awaiting the user's inline approval (interactive
    /// `ask` mode / Auto-mode escalation). The reducer enqueues a modal; the
    /// answer flows back as `Cmd::ResolveApproval`. The tool task is parked
    /// until then, so the turn naturally pauses (its outcome slot stays
    /// `None`).
    ApprovalRequested {
        turn: TurnId,
        call_id: ToolCallId,
        tool: String,
        risk: String,
        kind: ApprovalKind,
        prompt: String,
        allowlist_scope: String,
    },

    // ── MCP (from effect::mcp) ──────────────────────────────────────
    /// `initialize` succeeded; server is ready to dispatch tools.
    McpServerReady {
        name: String,
        tools: Vec<McpToolSpec>,
    },
    /// Server startup failed OR the child exited with non-zero.
    McpServerErrored {
        name: String,
        reason: String,
    },
    McpServerStopped {
        name: String,
    },

    // ── Persistence (from effect::persistence) ──────────────────────
    /// `MERMAID.md` loaded / changed / removed since last check.
    InstructionsChanged(Option<LoadedInstructions>),
    /// Memory files loaded / changed / removed since last check.
    MemoryChanged(Option<crate::app::memory::LoadedMemory>),
    /// `save_conversation` finished.
    SessionSaved,
    /// `/load <id>` — a saved conversation has been read off disk.
    ConversationLoaded(crate::session::ConversationHistory),
    /// Response to `Cmd::ListConversations`. Populates the `/load`
    /// picker's candidate list.
    ConversationsListed(Vec<ConversationSummary>),
    /// Response to `/tasks`.
    RuntimeTasksListed(Vec<TaskRecord>),
    /// Response to `/task <id>`.
    RuntimeTaskLoaded {
        task: Option<TaskRecord>,
        events: Vec<TaskTimelineEvent>,
    },
    /// Response to `/processes`.
    RuntimeProcessesListed(Vec<ProcessRecord>),
    /// Generic daemon/runtime text response.
    RuntimeText(String),
    RuntimeApprovalsListed(Vec<ApprovalRecord>),
    RuntimeCheckpointsListed(Vec<CheckpointRecord>),
    RuntimePluginsListed(Vec<PluginInstallRecord>),

    // ── Misc model operations ───────────────────────────────────────
    /// `/model <name>` finished pulling (Ollama only).
    ModelPullFinished {
        model: String,
    },
    /// Streaming stdout line from an `ollama pull` subprocess.
    /// Reducer forwards to the status line for the user to watch.
    ModelPullProgress(String),

    // ── Housekeeping ────────────────────────────────────────────────
    /// 1/60s timer tick. Used for spinner animation + elapsed-time
    /// display. Reducer only advances derived fields.
    Tick,
    /// Status line expired (self-clear) or user dismissed.
    StatusDismiss,
    /// Terminal was resized. Reducer normally no-ops; render consumes.
    Resize {
        width: u16,
        height: u16,
    },

    // ── Status feedback from async effects ─────────────────────────
    /// Set `state.status` to `(text, kind)` and schedule automatic
    /// dismissal after `dismiss_ms`. Used by effect handlers that
    /// need to surface user-visible feedback without a bespoke Msg
    /// per effect — today that's clipboard-read success / failure
    /// (F14), but the variant is general and other effects will reuse
    /// it. Reducer handles this arm by setting `state.status` and
    /// pushing `Cmd::DismissStatusAfter { ms: dismiss_ms }`.
    TransientStatus {
        text: String,
        kind: super::state::StatusKind,
        dismiss_ms: u64,
    },

    // ── Mouse (F13) ─────────────────────────────────────────────────
    /// Mouse-wheel scroll in the chat pane. Positive delta = scroll
    /// toward older messages (up), negative = toward newer (down). The
    /// reducer tracks the scroll offset on `ui.chat_scroll`; the
    /// ChatWidget reads it during render.
    MouseScroll {
        delta: i16,
    },
    /// Ctrl+Click on an image thumbnail in the chat pane. The
    /// coordinates are absolute screen row/col; the render cache's
    /// `ChatState::find_image_at_screen_pos` maps them to a
    /// `(message_index, image_index)` pair. The main loop handles the
    /// lookup before forwarding this message to the reducer, so by
    /// the time the reducer sees it, the target has already been
    /// resolved into a base64 payload and this Msg carries the
    /// already-decoded image. The reducer just emits
    /// `Cmd::WriteImageToTemp` + `Cmd::OpenInSystem`.
    OpenImageAt {
        message_index: usize,
        image_index: usize,
    },
    /// Copy the current chat text selection to the system clipboard. The main
    /// loop reads the selected text from the render layer (`rstate.chat`) and
    /// emits this, so the side effect flows through `update()` — and is recorded
    /// for replay — instead of being dispatched out-of-band (#18).
    CopySelection(String),
}

/// Bare key event — deliberately smaller than crossterm's `KeyEvent`
/// so the reducer doesn't depend on crossterm. The app event source
/// does the conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Key {
    pub code: KeyCode,
    pub modifiers: KeyMods,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyCode {
    Char(char),
    Enter,
    Escape,
    Backspace,
    Delete,
    Tab,
    BackTab,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    F(u8),
    /// Anything we don't care about (media keys, etc.).
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KeyMods {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl KeyMods {
    pub const NONE: Self = Self {
        ctrl: false,
        alt: false,
        shift: false,
    };

    pub const fn ctrl() -> Self {
        Self {
            ctrl: true,
            ..Self::NONE
        }
    }

    pub const fn alt() -> Self {
        Self {
            alt: true,
            ..Self::NONE
        }
    }

    pub fn is_empty(self) -> bool {
        !self.ctrl && !self.alt && !self.shift
    }
}

/// Paste payload. Images come in as raw bytes; text as UTF-8.
#[derive(Debug, Clone)]
pub enum Paste {
    Text(String),
    Image { bytes: Vec<u8>, format: String },
}

/// `/context` subcommands. No-arg shows the window; the rest tune the Ollama
/// context window (per-model, persisted), mirroring `/reasoning`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextCmd {
    /// Show the current window + usage (no arg).
    Show,
    /// `/context <n>` — set a per-model `num_ctx` override.
    Set(u32),
    /// `/context auto` — clear the override, return to auto-fit.
    Auto,
    /// `/context max` — use the model's full advertised window.
    Max,
    /// `/context offload on|off` — toggle Ollama RAM offload.
    Offload(bool),
}

/// Slash commands — a typed surface over what the user typed as
/// `/<name> [args]`. Parsed in `app::event_source` against the single
/// `COMMAND_REGISTRY`; unknown commands produce `SlashCmd::Unknown`
/// so the reducer can issue a "no such command" status line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCmd {
    /// No arg → show current; `Some` → switch (and pull if needed).
    Model(Option<String>),
    Reasoning(Option<ReasoningLevel>),
    VisibleReasoning(Option<String>),
    /// No arg → show current safety mode; `Some` → switch it for this
    /// session (`Shift+Tab` cycles the same field). Session-scoped.
    Safety(Option<SafetyMode>),
    Clear,
    Save(Option<String>),
    Load(Option<String>),
    List,
    Usage,
    Context(ContextCmd),
    Compact(Option<String>),
    /// List saved durable memories.
    Memory,
    /// Save free-text as a private memory.
    Remember(Option<String>),
    /// Delete a memory by name/id.
    Forget(Option<String>),
    /// Prune duplicate/obsolete memories via a one-shot model pass.
    ConsolidateMemory,
    Doctor,
    Tasks,
    Task(Option<String>),
    Pause(Option<String>),
    Resume(Option<String>),
    Cancel(Option<String>),
    Handoff(Option<String>),
    Report(Option<String>),
    Processes,
    Logs(Option<String>),
    Stop(Option<String>),
    Restart(Option<String>),
    Open(Option<String>),
    Ports,
    Approvals,
    Approve(Option<String>),
    Deny(Option<String>),
    Checkpoint(Option<String>),
    Checkpoints,
    Restore(Option<String>),
    ModelInfo(Option<String>),
    Plugins,
    CloudSetup,
    Help,
    Quit,
    /// User typed something that isn't in the registry; carries the
    /// raw name for the error message.
    Unknown(String),
}

impl Msg {
    /// Extract the `TurnId` for effect-result variants. Returns `None`
    /// for variants that aren't turn-scoped (user intent,
    /// housekeeping, MCP lifecycle). The reducer uses this to
    /// short-circuit stale events.
    pub fn turn_id(&self) -> Option<TurnId> {
        match self {
            Msg::StreamText { turn, .. }
            | Msg::StreamReasoning { turn, .. }
            | Msg::StreamToolCall { turn, .. }
            | Msg::ContextUsageEstimated { turn, .. }
            | Msg::CompactionFinished { turn, .. }
            | Msg::CompactionFailed { turn, .. }
            | Msg::StreamDone { turn, .. }
            | Msg::UpstreamError { turn, .. }
            | Msg::ToolStarted { turn, .. }
            | Msg::ToolProgress { turn, .. }
            | Msg::ToolFinished { turn, .. }
            | Msg::ApprovalRequested { turn, .. } => Some(*turn),
            Msg::TurnCancelled(turn) => Some(*turn),
            _ => None,
        }
    }

    /// Classification for telemetry / replay tooling. Cheaper than a
    /// full `Debug` string and stable across refactors.
    pub fn kind(&self) -> MsgKind {
        match self {
            Msg::Key(_) => MsgKind::Key,
            Msg::Paste(_) => MsgKind::Paste,
            Msg::SubmitPrompt { .. } => MsgKind::SubmitPrompt,
            Msg::Slash(_) => MsgKind::Slash,
            Msg::CancelTurn => MsgKind::CancelTurn,
            Msg::ConfirmAccepted | Msg::ConfirmDeclined => MsgKind::Confirm,
            Msg::Quit => MsgKind::Quit,
            Msg::RuntimeSignal(_) => MsgKind::RuntimeSignal,
            Msg::StreamText { .. } => MsgKind::StreamText,
            Msg::StreamReasoning { .. } => MsgKind::StreamReasoning,
            Msg::StreamToolCall { .. } => MsgKind::StreamToolCall,
            Msg::ContextUsageEstimated { .. } => MsgKind::ContextUsageEstimated,
            Msg::ProviderContextResolved { .. } => MsgKind::ProviderContextResolved,
            Msg::OllamaPlacementResolved { .. } => MsgKind::OllamaPlacementResolved,
            Msg::BuiltinToolSchemaTokens(_) => MsgKind::BuiltinToolSchemaTokens,
            Msg::CompactionFinished { .. } => MsgKind::CompactionFinished,
            Msg::CompactionFailed { .. } => MsgKind::CompactionFailed,
            Msg::StreamDone { .. } => MsgKind::StreamDone,
            Msg::UpstreamError { .. } => MsgKind::UpstreamError,
            Msg::ToolStarted { .. } => MsgKind::ToolStarted,
            Msg::ToolProgress { .. } => MsgKind::ToolProgress,
            Msg::ToolFinished { .. } => MsgKind::ToolFinished,
            Msg::ApprovalRequested { .. } => MsgKind::ApprovalRequested,
            Msg::TurnCancelled(_) => MsgKind::TurnCancelled,
            Msg::McpServerReady { .. }
            | Msg::McpServerErrored { .. }
            | Msg::McpServerStopped { .. } => MsgKind::Mcp,
            Msg::InstructionsChanged(_) => MsgKind::InstructionsChanged,
            Msg::MemoryChanged(_) => MsgKind::MemoryChanged,
            Msg::SessionSaved => MsgKind::SessionSaved,
            Msg::ConversationLoaded(_) => MsgKind::ConversationLoaded,
            Msg::ConversationsListed(_) => MsgKind::ConversationsListed,
            Msg::RuntimeTasksListed(_)
            | Msg::RuntimeTaskLoaded { .. }
            | Msg::RuntimeProcessesListed(_)
            | Msg::RuntimeText(_)
            | Msg::RuntimeApprovalsListed(_)
            | Msg::RuntimeCheckpointsListed(_)
            | Msg::RuntimePluginsListed(_) => MsgKind::RuntimeStore,
            Msg::ModelPullFinished { .. } => MsgKind::ModelPullFinished,
            Msg::ModelPullProgress(_) => MsgKind::ModelPullProgress,
            Msg::Tick => MsgKind::Tick,
            Msg::StatusDismiss => MsgKind::StatusDismiss,
            Msg::Resize { .. } => MsgKind::Resize,
            Msg::MouseScroll { .. } => MsgKind::MouseScroll,
            Msg::OpenImageAt { .. } => MsgKind::OpenImageAt,
            Msg::TransientStatus { .. } => MsgKind::TransientStatus,
            Msg::CopySelection(_) => MsgKind::CopySelection,
        }
    }
}

/// Compact kind tag for tracing / replay indexing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsgKind {
    Key,
    Paste,
    SubmitPrompt,
    Slash,
    CancelTurn,
    Confirm,
    Quit,
    RuntimeSignal,
    StreamText,
    StreamReasoning,
    StreamToolCall,
    ContextUsageEstimated,
    ProviderContextResolved,
    OllamaPlacementResolved,
    BuiltinToolSchemaTokens,
    CompactionFinished,
    CompactionFailed,
    StreamDone,
    UpstreamError,
    ToolStarted,
    ToolProgress,
    ToolFinished,
    ApprovalRequested,
    TurnCancelled,
    Mcp,
    InstructionsChanged,
    MemoryChanged,
    SessionSaved,
    ConversationLoaded,
    ConversationsListed,
    RuntimeStore,
    ModelPullFinished,
    ModelPullProgress,
    Tick,
    StatusDismiss,
    Resize,
    MouseScroll,
    OpenImageAt,
    TransientStatus,
    CopySelection,
}

/// Helper for `app::event_source` — pass through the MCP config that
/// effect::mcp needs to dispatch `InitMcpServers` as its first effect.
/// Not a `Msg` because it's startup-only.
#[derive(Debug, Clone)]
pub struct StartupConfig {
    pub mcp_servers: std::collections::HashMap<String, McpServerConfig>,
    pub cwd: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_id_extracted_from_stream_messages() {
        let m = Msg::StreamText {
            turn: TurnId(7),
            chunk: "hi".to_string(),
        };
        assert_eq!(m.turn_id(), Some(TurnId(7)));
    }

    #[test]
    fn turn_id_none_for_user_intent() {
        let m = Msg::CancelTurn;
        assert_eq!(m.turn_id(), None);
        let m = Msg::Quit;
        assert_eq!(m.turn_id(), None);
        let m = Msg::Tick;
        assert_eq!(m.turn_id(), None);
    }

    #[test]
    fn turn_id_none_for_mcp_lifecycle() {
        let m = Msg::McpServerReady {
            name: "s".to_string(),
            tools: vec![],
        };
        assert_eq!(m.turn_id(), None);
    }

    #[test]
    fn key_mods_builder_defaults_match_const() {
        assert_eq!(KeyMods::default(), KeyMods::NONE);
        assert!(KeyMods::ctrl().ctrl);
        assert!(!KeyMods::ctrl().alt);
        assert!(!KeyMods::ctrl().shift);
    }

    #[test]
    fn kind_stable_across_variants() {
        assert_eq!(Msg::Quit.kind(), MsgKind::Quit);
        assert_eq!(Msg::Tick.kind(), MsgKind::Tick);
        assert_eq!(
            Msg::StreamText {
                turn: TurnId(1),
                chunk: String::new()
            }
            .kind(),
            MsgKind::StreamText
        );
    }

    #[test]
    fn slash_cmd_carries_none_for_no_arg() {
        let c = SlashCmd::Model(None);
        assert_eq!(c, SlashCmd::Model(None));
        assert_ne!(c, SlashCmd::Model(Some("ollama/qwen3".to_string())));
    }
}
