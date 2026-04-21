//! The single state shape for the whole application.
//!
//! `State` is the value the reducer operates on. Everything the UI
//! shows — from the chat log to the input buffer to the "Thinking…"
//! animation — is derived from fields in this struct. Mutation happens
//! only inside `update(state, msg)`; no other code in the new v0.7
//! architecture is allowed to hold a `&mut State`.
//!
//! The sub-state enums (`TurnState`, `UiMode`, `McpServerStatus`) are
//! intentionally explicit sum types. A previous generation of this
//! codebase used bools like `is_generating: bool`, `is_cancelling:
//! bool`, `is_tool_call_pending: bool` — the invariants between those
//! bools were load-bearing and enforced by convention. Expressing the
//! same state as a single enum makes it impossible to be in two modes
//! at once, and the reducer can pattern-match instead of guarding with
//! if-chains.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::SystemTime;

use crate::app::instructions::LoadedInstructions;
use crate::app::{Config, McpServerConfig};
use crate::models::ChatMessage;
use crate::models::ReasoningLevel;
use crate::models::tool_call::ToolCall as ModelToolCall;
use crate::session::ConversationHistory;

use super::ids::{IdAllocator, ToolCallId, TurnId};
use super::msg::Msg;

/// Root state. The reducer takes `State` by value, returns a new
/// `State`, and emits any side-effects as a `Vec<Cmd>`. No `&mut` — a
/// deliberate choice so tests can diff before/after without aliasing
/// worries, and so replay ("compute the final State that this Msg log
/// would produce") is a straight fold.
#[derive(Debug, Clone)]
pub struct State {
    pub session: Session,
    pub turn: TurnState,
    pub ui: UiState,
    pub mcp: McpState,
    pub settings: Config,
    pub instructions: Option<LoadedInstructions>,
    /// Current working directory. Captured once at startup; tools
    /// receive it via `ExecContext::workdir` and spawned subprocesses
    /// inherit it. Centralized here so tests can inject a fake cwd.
    pub cwd: PathBuf,
    pub ids: IdAllocatorBundle,
    /// When `Some`, the next render should pop up a modal confirmation
    /// (e.g. "are you sure you want to /clear?"). Cleared by the
    /// reducer when the user answers.
    pub confirm: Option<Confirmation>,
    /// Transient status line under the input box. One-shot — cleared by
    /// `Msg::StatusConsumed` or by the next rendered frame depending on
    /// `StatusKind`.
    pub status: Option<StatusLine>,
    /// Quit flag. When set, the main loop drains pending effects and
    /// exits. The reducer never panics on its own; it sets this instead.
    pub should_exit: bool,
}

impl State {
    /// Build a fresh state tied to a specific model + project dir.
    /// Nothing about this touches the filesystem or tokio — pure.
    pub fn new(settings: Config, cwd: PathBuf, model_id: String) -> Self {
        let project_path = cwd.display().to_string();
        let conversation = ConversationHistory::new(project_path, model_id.clone());
        let initial_title = conversation.title.clone();
        Self {
            session: Session {
                conversation,
                model_id,
                reasoning: settings.default_model.reasoning,
                cumulative_tokens: 0,
            },
            turn: TurnState::Idle,
            ui: UiState {
                last_title_dispatched: Some(initial_title),
                ..UiState::default()
            },
            mcp: McpState::default(),
            settings,
            instructions: None,
            cwd,
            ids: IdAllocatorBundle::default(),
            confirm: None,
            status: None,
            should_exit: false,
        }
    }

    /// True iff the reducer is currently mid-turn. UI uses this for
    /// the "⏎ cancels generation" hint and for keybind routing.
    pub fn is_busy(&self) -> bool {
        !matches!(self.turn, TurnState::Idle)
    }

    /// The active `TurnId`, if any turn is in flight. The reducer
    /// filters incoming effect messages by comparing their embedded
    /// `TurnId` to this value — if the user cancelled and started a
    /// new turn, stale results from the old turn are dropped cleanly.
    pub fn current_turn_id(&self) -> Option<TurnId> {
        self.turn.id()
    }
}

/// Persistent conversational state that survives across turns.
///
/// "Session" here means the user-visible chat session, not the tokio
/// runtime or the TCP connection to the provider. One chat = one
/// `Session` = one on-disk `ConversationHistory` file.
#[derive(Debug, Clone)]
pub struct Session {
    pub conversation: ConversationHistory,
    pub model_id: String,
    pub reasoning: ReasoningLevel,
    /// Running total of tokens consumed across every turn in this
    /// session. The reducer adds `StreamDone.usage.total_tokens` on
    /// each successful turn end; status widget reads it for the
    /// bottom-right counter.
    pub cumulative_tokens: usize,
}

impl Session {
    /// The committed message log. All messages visible in the chat
    /// widget live here; partial in-flight content lives in
    /// `TurnState::Generating`.
    pub fn messages(&self) -> &[ChatMessage] {
        &self.conversation.messages
    }

    /// Append a committed assistant/user/tool message. Mutation happens
    /// through here so the reducer has one chokepoint to update the
    /// conversation's `updated_at` and derived title. Pure — no I/O.
    pub fn append(&mut self, msg: ChatMessage) {
        self.conversation.add_messages(&[msg]);
    }
}

/// The turn state machine. Each variant carries its own `TurnId` so
/// the reducer can cheaply check "is this effect result for the
/// current turn?" without threading the ID through every match arm.
///
/// The `ExecutingTools::outcomes: Vec<Option<ToolOutcome>>` field is
/// the architectural payoff: every slot starts `None`, flips to
/// `Some(outcome)` as each tool finishes, and the transition to the
/// follow-up `Generating` state requires `outcomes` to be fully
/// populated. Statically impossible to "lose" a tool result.
#[derive(Debug, Clone)]
pub enum TurnState {
    Idle,
    Generating {
        id: TurnId,
        started: SystemTime,
        partial_text: String,
        partial_reasoning: String,
        /// Running token estimate — updated by `StreamText` events.
        tokens: usize,
        /// Sub-phase for richer status display (see `GenPhase`).
        phase: GenPhase,
        /// Anthropic-only: carries forward across the turn so we can
        /// attach it to the committed assistant message. `None` until
        /// the Anthropic adapter emits a signature event.
        thinking_signature: Option<String>,
        /// Tool calls the model has streamed so far this turn.
        /// `StreamToolCall` messages push here; `StreamDone` drains
        /// the vec, allocates `PendingToolCall` entries, and
        /// transitions to `ExecutingTools`. When the vec is empty at
        /// stream end, the turn returns to `Idle`.
        pending_tool_calls: Vec<ModelToolCall>,
    },
    ExecutingTools {
        id: TurnId,
        calls: Vec<PendingToolCall>,
        outcomes: Vec<Option<ToolOutcome>>,
    },
    /// `CancelTurn` was dispatched. The reducer has already emitted a
    /// `Cmd::CancelScope` — now we wait for the final `Cancelled` /
    /// `StreamDone` that the effect runner sends back when the scope's
    /// `JoinSet` drains. Only then do we transition to `Idle`.
    ///
    /// Stuck in `Cancelling` too long = effect runner has a bug. UI
    /// surfaces a "cleanup taking a while…" hint after 2s.
    Cancelling {
        id: TurnId,
        since: SystemTime,
    },
}

impl TurnState {
    pub fn id(&self) -> Option<TurnId> {
        match self {
            TurnState::Idle => None,
            TurnState::Generating { id, .. }
            | TurnState::ExecutingTools { id, .. }
            | TurnState::Cancelling { id, .. } => Some(*id),
        }
    }

    /// True when a `Msg` tagged with the given `TurnId` should be
    /// accepted. Events from prior turns return false — the reducer's
    /// first line on every effect-result arm.
    pub fn accepts(&self, event_turn: TurnId) -> bool {
        self.id() == Some(event_turn)
    }
}

/// Sub-phase of `Generating`. Informational — the reducer updates it
/// as the provider's stream progresses so the UI can show a meaningful
/// status ("Thinking…" vs "Sending…" vs "Streaming").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenPhase {
    /// Request dispatched, awaiting first byte.
    Sending,
    /// First chunk was reasoning content — currently inside a
    /// thinking/reasoning block.
    Thinking,
    /// Streaming assistant content (post-thinking, or no thinking at
    /// all).
    Streaming,
}

/// One pending tool call that the model has asked us to execute. Wraps
/// the wire-format tool call with an internal ID + the original
/// provider-native structure so the reducer never loses provenance.
#[derive(Debug, Clone)]
pub struct PendingToolCall {
    pub call_id: ToolCallId,
    /// The raw tool call as it appeared in the model's response.
    /// Preserved verbatim so the follow-up tool-result message can
    /// reference the right function name + id on the wire.
    pub source: ModelToolCall,
}

/// Outcome of a single tool execution. The `Cancelled` variant is
/// distinct from `Error` on purpose: cancellation is a user-initiated
/// abort with a different UX (no error toast, no retry suggestion),
/// not an error.
#[derive(Debug, Clone)]
pub enum ToolOutcome {
    Finished {
        output: String,
        /// For multimodal tools (screenshots) — base64-encoded images
        /// returned alongside the textual output.
        images: Option<Vec<String>>,
        /// Wall-clock duration, for UI display.
        duration_secs: f64,
    },
    Error {
        error: String,
        duration_secs: f64,
    },
    /// The scope's `CancellationToken` fired before the tool produced
    /// a result. Tool's child processes (if any) were killed via
    /// `kill_on_drop`.
    Cancelled,
}

impl ToolOutcome {
    pub fn was_cancelled(&self) -> bool {
        matches!(self, ToolOutcome::Cancelled)
    }

    /// Convert to a textual representation suitable for embedding in
    /// the follow-up `tool` role message. Cancellation produces a
    /// placeholder so the model sees "this was skipped" rather than
    /// the history becoming malformed.
    pub fn as_tool_message_content(&self) -> String {
        match self {
            ToolOutcome::Finished { output, .. } => output.clone(),
            ToolOutcome::Error { error, .. } => format!("Error: {}", error),
            ToolOutcome::Cancelled => {
                "[Tool call skipped: the user cancelled before execution]".to_string()
            },
        }
    }
}

/// All UI-only state. Things in `UiState` never affect what gets sent
/// to the model — only what the user sees.
#[derive(Debug, Clone, Default)]
pub struct UiState {
    pub mode: UiMode,
    pub input_buffer: String,
    /// Byte position within `input_buffer`. The reducer normalizes to
    /// a UTF-8 char boundary on every mutation via
    /// `floor_char_boundary`, so widgets can slice safely.
    pub input_cursor: usize,
    /// Pending image pastes queued for the next user message.
    pub attachments: Vec<Attachment>,
    /// When true, keyboard focus is on the attachment bar (up arrow
    /// from input moves focus up here; Esc returns focus to input).
    pub attachment_focused: bool,
    /// Highlighted attachment index when focused. Ignored when
    /// `attachment_focused` is false.
    pub attachment_selected: usize,
    /// Scroll offset for the chat pane.
    pub chat_scroll: usize,
    /// When the slash-palette is open, this holds the filter prefix
    /// (typed after the leading `/`) so the palette widget can
    /// re-query the registry.
    pub palette_filter: String,
    /// When `Some(i)`, the palette has a highlighted row. `None` =
    /// closed / not showing.
    pub palette_cursor: Option<usize>,
    /// Messages the user typed while a turn was in flight. The
    /// reducer pops the oldest and auto-submits on a successful
    /// `StreamDone`. FIFO order.
    pub queued_messages: VecDeque<String>,
    /// Last terminal title dispatched via `Cmd::SetTerminalTitle`.
    /// Arms that change `session.conversation.title` consult this
    /// and emit a fresh `SetTerminalTitle` only on diff.
    pub last_title_dispatched: Option<String>,
    /// Follow-up `Msg`s the reducer has queued for re-entry. The
    /// outer `update()` drains this after each single-step call so
    /// a handler can emit a synthetic event (e.g. Enter-on-slash
    /// queuing `Msg::Slash(cmd)`) without self-invoking the
    /// reducer. Bounded drain depth guards against runaway loops.
    pub pending_msgs: VecDeque<Msg>,
    /// Up-arrow history navigation cursor into
    /// `session.conversation.input_history`. `None` = not
    /// navigating (input_buffer is whatever the user typed).
    /// `Some(i)` = currently displaying history entry at index `i`
    /// from the END (0 = newest).
    pub input_history_cursor: Option<usize>,
    /// Whatever the user had typed before hitting Up. Preserved so
    /// stepping past the newest history entry with Down restores
    /// the partial input unchanged. Cleared on any non-nav key.
    pub history_draft: String,
}

/// Top-level UI mode. Like `TurnState` this is a sum type instead of a
/// zoo of independent bools. `EditingInput` is the default.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum UiMode {
    #[default]
    EditingInput,
    /// Slash-command palette open (user typed `/`).
    Palette,
    /// `/load` — list of saved conversations visible. `candidates`
    /// holds what the effect handler returned; `cursor` is the
    /// highlighted row.
    ConversationList {
        candidates: Vec<ConversationSummary>,
        cursor: usize,
    },
    /// `/model` — list of available models visible.
    ModelList,
}

/// Summary row for the conversation picker. Produced by
/// `Cmd::ListConversations` → `Msg::ConversationsListed`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationSummary {
    pub id: String,
    pub title: String,
    pub message_count: usize,
    pub updated_at: String,
}

/// One pasted image, ready to send. Kept in the reducer state — not on
/// disk — because the image hasn't been confirmed for a message yet.
#[derive(Debug, Clone)]
pub struct Attachment {
    pub id: u64,
    pub base64_data: String,
    /// Temp file path (written by the effect runner when the paste
    /// event comes in, so the TUI can show a preview).
    pub temp_path: PathBuf,
    pub size_bytes: usize,
    pub format: String,
}

/// MCP server lifecycle state. Mutation is driven by `Msg::McpServer*`
/// events emitted from `effect::mcp` when a server starts, advertises
/// tools, or exits.
#[derive(Debug, Clone, Default)]
pub struct McpState {
    pub servers: HashMap<String, McpServerEntry>,
}

#[derive(Debug, Clone)]
pub struct McpServerEntry {
    pub config: McpServerConfig,
    pub status: McpServerStatus,
    /// Tools advertised by the server. Populated on the
    /// `McpServerReady` event; reducer exposes these to the model
    /// when building the tool list for the next request.
    pub tools: Vec<McpToolSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpServerStatus {
    /// `initialize` request dispatched, not yet acknowledged.
    Starting,
    Ready,
    Errored {
        reason: String,
    },
    Stopped,
}

/// Subset of the MCP `ToolDefinition` carried in reducer state. The
/// reducer doesn't need the full schema; the effect layer uses the
/// server name + tool name to route, and the reducer uses the
/// description for palette display.
#[derive(Debug, Clone)]
pub struct McpToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// A pending user confirmation (modal). Examples: confirming `/clear`,
/// confirming overwrite of an existing file on `/save <name>`.
#[derive(Debug, Clone)]
pub struct Confirmation {
    pub prompt: String,
    pub accept_msg_token: ConfirmationTarget,
}

/// What to do when the user confirms. The reducer translates
/// `Msg::ConfirmAccepted` into a secondary dispatch based on this.
#[derive(Debug, Clone)]
pub enum ConfirmationTarget {
    ClearConversation,
    OverwriteSavedConversation { name: String },
}

/// Transient status line shown under the input box. Self-clears after
/// its kind's expected lifetime — `Persistent` entries stay until
/// explicitly dismissed.
#[derive(Debug, Clone)]
pub struct StatusLine {
    pub text: String,
    pub kind: StatusKind,
    pub shown_at: SystemTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusKind {
    Info,
    Warn,
    Error,
    /// Stays until the next turn or explicit dismissal.
    Persistent,
}

/// All ID allocators for the session. Grouped so the reducer can
/// request any of them through a single `&mut state.ids`.
#[derive(Debug, Clone, Copy, Default)]
pub struct IdAllocatorBundle {
    pub turn: IdAllocator,
    pub tool_call: IdAllocator,
}

impl IdAllocatorBundle {
    pub fn fresh_turn(&mut self) -> TurnId {
        TurnId(self.turn.next())
    }

    pub fn fresh_tool_call(&mut self) -> ToolCallId {
        ToolCallId(self.tool_call.next())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mock_state() -> State {
        State::new(
            Config::default(),
            PathBuf::from("/tmp/project"),
            "ollama/test".to_string(),
        )
    }

    #[test]
    fn fresh_state_is_idle() {
        let s = mock_state();
        assert!(matches!(s.turn, TurnState::Idle));
        assert!(!s.is_busy());
        assert!(s.current_turn_id().is_none());
    }

    #[test]
    fn turn_state_accepts_matches_id() {
        let s = TurnState::Generating {
            id: TurnId(7),
            started: SystemTime::now(),
            partial_text: String::new(),
            partial_reasoning: String::new(),
            tokens: 0,
            phase: GenPhase::Sending,
            thinking_signature: None,
            pending_tool_calls: Vec::new(),
        };
        assert!(s.accepts(TurnId(7)));
        assert!(!s.accepts(TurnId(6)));
        assert!(!s.accepts(TurnId(8)));
    }

    #[test]
    fn idle_rejects_all_turn_ids() {
        let s = TurnState::Idle;
        assert!(!s.accepts(TurnId(1)));
        assert!(!s.accepts(TurnId(999)));
    }

    #[test]
    fn fresh_id_allocators_monotonic() {
        let mut bundle = IdAllocatorBundle::default();
        assert_eq!(bundle.fresh_turn(), TurnId(1));
        assert_eq!(bundle.fresh_turn(), TurnId(2));
        assert_eq!(bundle.fresh_tool_call(), ToolCallId(1));
        // Cross-allocator independence — fresh turns don't consume
        // tool call IDs.
    }

    #[test]
    fn tool_outcome_cancelled_content_is_placeholder() {
        let o = ToolOutcome::Cancelled;
        assert!(o.was_cancelled());
        let content = o.as_tool_message_content();
        assert!(content.contains("cancelled"));
    }

    #[test]
    fn tool_outcome_finished_returns_output_verbatim() {
        let o = ToolOutcome::Finished {
            output: "hello world".to_string(),
            images: None,
            duration_secs: 0.1,
        };
        assert_eq!(o.as_tool_message_content(), "hello world");
        assert!(!o.was_cancelled());
    }

    #[test]
    fn session_append_records_message() {
        let mut s = mock_state();
        s.session.append(ChatMessage::user("hi"));
        assert_eq!(s.session.messages().len(), 1);
        assert_eq!(s.session.messages()[0].content, "hi");
    }
}
