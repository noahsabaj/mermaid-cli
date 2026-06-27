//! The single state shape for the whole application.
//!
//! `State` is the value the reducer operates on. Everything the UI
//! shows — from the chat log to the input buffer to the "Thinking…"
//! animation — is derived from fields in this struct. Mutation happens
//! only inside `update(state, msg)`; no other code is allowed to hold
//! a `&mut State`.
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

use chrono::{DateTime, Local};

use crate::app::instructions::LoadedInstructions;
use crate::app::{Config, McpServerConfig};
use crate::models::ChatMessage;
use crate::models::tool_call::ToolCall as ModelToolCall;
use crate::models::{ReasoningLevel, TokenUsage, TokenUsageSource};
use crate::runtime::SafetyMode;
use crate::session::ConversationHistory;

use super::cmd::ChatRequest;
use super::compaction::CompactionTrigger;
use super::ids::{IdAllocator, ToolCallId, TurnId};
use super::msg::Msg;
use super::runtime::{RuntimeState, ToolArtifact, ToolRunMetadata, ToolStatus};

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
    /// Durable semantic memory snapshot (auto-derived index + entries),
    /// refreshed per turn like `instructions`. Its index is injected into the
    /// model prompt alongside project instructions.
    pub memory: Option<crate::app::memory::LoadedMemory>,
    /// Current working directory. Captured once at startup; tools
    /// receive it via `ExecContext::workdir` and spawned subprocesses
    /// inherit it. Centralized here so tests can inject a fake cwd.
    pub cwd: PathBuf,
    pub ids: IdAllocatorBundle,
    /// When `Some`, the next render should pop up a modal confirmation
    /// (e.g. "are you sure you want to /clear?"). Cleared by the
    /// reducer when the user answers.
    pub confirm: Option<Confirmation>,
    /// FIFO queue of tool actions awaiting the user's inline approval
    /// (interactive `ask` mode + Auto-mode escalations). The front item is
    /// rendered as a modal; answering it pops the item and emits
    /// `Cmd::ResolveApproval`, which unblocks the parked tool task. Empty in
    /// headless mode (no broker → the out-of-band `/approve` flow instead).
    pub pending_approval: VecDeque<PendingApproval>,
    /// Transient status line under the input box. One-shot — cleared by
    /// `Msg::StatusConsumed` or by the next rendered frame depending on
    /// `StatusKind`.
    pub status: Option<StatusLine>,
    /// Runtime-only observability state: process registry, provider
    /// capability snapshot, and lifecycle timeline. Not sent to the
    /// model.
    pub runtime: RuntimeState,
    /// Quit flag. When set, the main loop drains pending effects and
    /// exits. The reducer never panics on its own; it sets this instead.
    pub should_exit: bool,
    /// Wall-clock for the current reducer step, injected as data (Cause 3).
    /// The driver stamps this once per tick — `Local::now()` live, or the
    /// recorded entry's `ts` on replay — *before* calling `update`. The
    /// reducer and the `transition` helpers read `state.now` instead of
    /// `Local::now()` / `SystemTime::now()`, so `update(State, Msg)` is a pure
    /// function of its inputs: the same `(State, Msg)` always yields the same
    /// `State`, and folding a recorded `Msg` log recomputes State exactly.
    pub now: DateTime<Local>,
}

impl State {
    /// Build a fresh state tied to a specific model + project dir.
    /// Nothing about this touches the filesystem or tokio — pure.
    pub fn new(settings: Config, cwd: PathBuf, model_id: String) -> Self {
        let project_path = cwd.display().to_string();
        let conversation = ConversationHistory::new(project_path, model_id.clone());
        let initial_title = conversation.title.clone();
        // F5: seed `mcp.servers` from the user's configured MCP
        // servers with `Starting` status. Previously the map started
        // empty, and `McpServerReady` handlers used `get_mut` —
        // configured servers never populated, so their tools never
        // reached `build_chat_request`'s outgoing tool list.
        let mcp = {
            let mut m = McpState::default();
            for (name, cfg) in &settings.mcp_servers {
                m.servers.insert(
                    name.clone(),
                    McpServerEntry {
                        config: cfg.clone(),
                        status: McpServerStatus::Starting,
                        tools: Vec::new(),
                    },
                );
            }
            m
        };
        // F11: honor the per-model reasoning preference (persisted via
        // `/reasoning high` while using a specific model). Falls back to
        // the global default when no entry exists.
        let reasoning = settings
            .reasoning_per_model
            .get(&model_id)
            .copied()
            .unwrap_or(settings.default_model.reasoning);
        let runtime = RuntimeState::new(&model_id);
        Self {
            session: Session {
                conversation,
                model_id,
                reasoning,
                safety_mode: settings.safety.mode,
                cumulative_tokens: 0,
                last_token_usage: None,
                cumulative_token_usage: TokenUsageTotals::default(),
                context_usage: None,
            },
            turn: TurnState::Idle,
            ui: UiState {
                last_title_dispatched: Some(initial_title),
                ..UiState::default()
            },
            mcp,
            settings,
            instructions: None,
            memory: None,
            cwd,
            ids: IdAllocatorBundle::default(),
            confirm: None,
            pending_approval: VecDeque::new(),
            status: None,
            runtime,
            should_exit: false,
            // Seed the injected clock so a freshly-built State is usable before
            // the driver's first per-tick stamp. The driver overwrites this on
            // every iteration (Cause 3); the reducer never reads the wall clock
            // directly.
            now: Local::now(),
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

/// Prompt/completion/total token counts normalized for UI display.
/// Providers report usage per API request; the session keeps both the
/// last request and the cumulative API usage so the footer does not
/// imply this is the current model context length.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsageTotals {
    pub prompt_tokens: usize,
    pub completion_tokens: usize,
    pub total_tokens: usize,
    pub cached_input_tokens: usize,
    pub cache_creation_input_tokens: usize,
    pub reasoning_output_tokens: usize,
}

impl TokenUsageTotals {
    pub fn from_usage(usage: &TokenUsage) -> Self {
        Self {
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            total_tokens: usage.total_tokens,
            cached_input_tokens: usage.cached_input_tokens,
            cache_creation_input_tokens: usage.cache_creation_input_tokens,
            reasoning_output_tokens: usage.reasoning_output_tokens,
        }
    }

    pub fn add_assign(&mut self, other: Self) {
        self.prompt_tokens = self.prompt_tokens.saturating_add(other.prompt_tokens);
        self.completion_tokens = self
            .completion_tokens
            .saturating_add(other.completion_tokens);
        self.total_tokens = self.total_tokens.saturating_add(other.total_tokens);
        self.cached_input_tokens = self
            .cached_input_tokens
            .saturating_add(other.cached_input_tokens);
        self.cache_creation_input_tokens = self
            .cache_creation_input_tokens
            .saturating_add(other.cache_creation_input_tokens);
        self.reasoning_output_tokens = self
            .reasoning_output_tokens
            .saturating_add(other.reasoning_output_tokens);
    }

    pub fn input_total_tokens(&self) -> usize {
        self.prompt_tokens
            .saturating_add(self.cached_input_tokens)
            .saturating_add(self.cache_creation_input_tokens)
    }

    pub fn output_total_tokens(&self) -> usize {
        self.completion_tokens
            .saturating_add(self.reasoning_output_tokens)
    }
}

/// Approximate request-context breakdown used before provider usage
/// arrives. These numbers are diagnostic estimates, not billing facts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PromptTokenBreakdown {
    pub system_tokens: usize,
    pub instructions_tokens: usize,
    pub message_tokens: usize,
    pub tool_schema_tokens: usize,
    pub image_count: usize,
    pub message_count: usize,
    pub tool_count: usize,
}

impl PromptTokenBreakdown {
    pub fn total_tokens(&self) -> usize {
        self.system_tokens
            .saturating_add(self.instructions_tokens)
            .saturating_add(self.message_tokens)
            .saturating_add(self.tool_schema_tokens)
    }
}

/// The model-visible context for the latest request. This is separate
/// from cumulative session usage, which is an API/accounting total.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextUsageSnapshot {
    pub used_tokens: usize,
    pub max_tokens: Option<usize>,
    pub remaining_tokens: Option<usize>,
    pub used_percent: Option<u8>,
    pub source: TokenUsageSource,
    pub prompt_tokens: usize,
    pub cached_input_tokens: usize,
    pub cache_creation_input_tokens: usize,
    pub completion_tokens: usize,
    pub reasoning_output_tokens: usize,
    pub breakdown: Option<PromptTokenBreakdown>,
}

impl ContextUsageSnapshot {
    pub fn from_usage(usage: &TokenUsage, max_tokens: Option<usize>) -> Self {
        Self::new(
            usage.total_tokens,
            max_tokens,
            usage.source,
            usage.prompt_tokens,
            usage.cached_input_tokens,
            usage.cache_creation_input_tokens,
            usage.completion_tokens,
            usage.reasoning_output_tokens,
            None,
        )
    }

    pub fn from_estimate(breakdown: PromptTokenBreakdown, max_tokens: Option<usize>) -> Self {
        let used = breakdown.total_tokens();
        Self::new(
            used,
            max_tokens,
            TokenUsageSource::Estimate,
            used,
            0,
            0,
            0,
            0,
            Some(breakdown),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn new(
        used_tokens: usize,
        max_tokens: Option<usize>,
        source: TokenUsageSource,
        prompt_tokens: usize,
        cached_input_tokens: usize,
        cache_creation_input_tokens: usize,
        completion_tokens: usize,
        reasoning_output_tokens: usize,
        breakdown: Option<PromptTokenBreakdown>,
    ) -> Self {
        let remaining_tokens = max_tokens.map(|max| max.saturating_sub(used_tokens));
        let used_percent = max_tokens
            .filter(|max| *max > 0)
            .map(|max| ((used_tokens.saturating_mul(100)) / max).min(100) as u8);
        Self {
            used_tokens,
            max_tokens,
            remaining_tokens,
            used_percent,
            source,
            prompt_tokens,
            cached_input_tokens,
            cache_creation_input_tokens,
            completion_tokens,
            reasoning_output_tokens,
            breakdown,
        }
    }

    pub fn is_estimate(&self) -> bool {
        self.source == TokenUsageSource::Estimate
    }

    /// Return a copy with `extra` tokens folded into the running total. Used by
    /// `/context` to add built-in tool-schema tokens that the reducer's
    /// MCP-only request estimate can't see. Recomputes `remaining_tokens` and
    /// `used_percent`; the breakdown is left untouched (the caller surfaces the
    /// built-in figure on its own line so the MCP line stays accurate).
    pub fn with_additional_tokens(mut self, extra: usize) -> Self {
        if extra == 0 {
            return self;
        }
        self.used_tokens = self.used_tokens.saturating_add(extra);
        self.remaining_tokens = self
            .max_tokens
            .map(|max| max.saturating_sub(self.used_tokens));
        self.used_percent = self
            .max_tokens
            .filter(|max| *max > 0)
            .map(|max| ((self.used_tokens.saturating_mul(100)) / max).min(100) as u8);
        self
    }
}

pub fn estimate_context_usage_for_request(
    request: &ChatRequest,
    max_tokens: Option<usize>,
) -> ContextUsageSnapshot {
    let system_tokens = approx_tokens(&request.system_prompt);
    let instructions_tokens = request
        .instructions
        .as_deref()
        .map(approx_tokens)
        .unwrap_or(0);
    let message_tokens = request
        .messages
        .iter()
        .map(|msg| {
            let image_chars = msg
                .images
                .as_ref()
                .map(|imgs| imgs.iter().map(|img| img.len()).sum::<usize>())
                .unwrap_or(0);
            // Include assistant tool-call name + arguments JSON, which the
            // estimate previously ignored (see estimate_message_tokens).
            let tool_call_chars = msg
                .tool_calls
                .as_ref()
                .map(|calls| {
                    calls
                        .iter()
                        .map(|tc| {
                            tc.function.name.len()
                                + tc.function.arguments.to_string().len()
                                + tc.id.as_deref().map(str::len).unwrap_or(0)
                        })
                        .sum::<usize>()
                })
                .unwrap_or(0);
            approx_tokens(&msg.content)
                .saturating_add(approx_tokens(&format!(
                    "{:?}{}{}",
                    msg.role,
                    msg.tool_name.as_deref().unwrap_or(""),
                    msg.tool_call_id.as_deref().unwrap_or("")
                )))
                .saturating_add(image_chars.div_ceil(4))
                .saturating_add(tool_call_chars.div_ceil(4))
        })
        .sum();
    let tool_schema_tokens = estimate_tool_schema_tokens(&request.tools);
    let image_count = request
        .messages
        .iter()
        .filter_map(|msg| msg.images.as_ref())
        .map(Vec::len)
        .sum();
    ContextUsageSnapshot::from_estimate(
        PromptTokenBreakdown {
            system_tokens,
            instructions_tokens,
            message_tokens,
            tool_schema_tokens,
            image_count,
            message_count: request.messages.len(),
            tool_count: request.tools.len(),
        },
        max_tokens,
    )
}

fn approx_tokens(text: &str) -> usize {
    text.len().div_ceil(4)
}

/// Estimate the token cost of a set of tool schemas as the model sees them
/// (serialized OpenAI-style). Shared by `estimate_context_usage_for_request`
/// and the effect runner so the reducer's `/context` preview can account for
/// the built-in tool schemas that are only appended to the request during
/// dispatch enrichment.
pub fn estimate_tool_schema_tokens(tools: &[super::cmd::ToolDefinition]) -> usize {
    let tool_schema: Vec<_> = tools.iter().map(|tool| tool.to_openai_json()).collect();
    serde_json::to_string(&tool_schema)
        .map(|s| approx_tokens(&s))
        .unwrap_or(0)
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
    /// Live safety mode for this session. Initialized from
    /// `config.safety.mode`, then mutated in-session by `Shift+Tab` /
    /// `/safety` (session-scoped — never written back to the config file).
    /// The reducer threads this into `Cmd::ExecuteTool` so the policy gate
    /// enforces the *current* mode, not the startup snapshot.
    pub safety_mode: SafetyMode,
    /// Running total of tokens consumed across every API request in
    /// this session. Kept for CLI JSON compatibility; the richer
    /// prompt/completion breakdown lives in `cumulative_token_usage`.
    pub cumulative_tokens: usize,
    /// Token usage for the most recent completed provider request.
    /// `None` means the provider did not report usage for that turn.
    pub last_token_usage: Option<TokenUsageTotals>,
    /// Prompt/completion/total API usage accumulated for this session.
    pub cumulative_token_usage: TokenUsageTotals,
    /// Latest model-visible context snapshot. This may be an estimate
    /// while a request is in flight and is replaced by provider-reported
    /// usage when available.
    pub context_usage: Option<ContextUsageSnapshot>,
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
        /// When tool execution started, so the status line can show elapsed
        /// time (a long-running command — `npm run dev`, a slow build — would
        /// otherwise look frozen at 0s).
        started: SystemTime,
        calls: Vec<PendingToolCall>,
        outcomes: Vec<Option<ToolOutcome>>,
    },
    /// A manual `/compact` request is summarizing history. Auto
    /// compaction runs while `Generating` because it is preflight for
    /// the same user turn; this variant is only for explicit user
    /// compaction.
    Compacting {
        id: TurnId,
        started: SystemTime,
        trigger: CompactionTrigger,
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
            | TurnState::Compacting { id, .. }
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

/// Outcome of a single tool execution.
///
/// `model_content` is the text that goes back to the model in the
/// follow-up tool message. Everything else is Mermaid-owned
/// structure for rendering, replay, process tracking, and timeline
/// inspection.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutcome {
    pub status: ToolStatus,
    pub summary: String,
    pub model_content: String,
    pub error: Option<String>,
    pub metadata: Box<ToolRunMetadata>,
    pub artifacts: Vec<ToolArtifact>,
    pub duration_secs: Option<f64>,
}

impl ToolOutcome {
    pub fn success(
        model_content: impl Into<String>,
        summary: impl Into<String>,
        duration_secs: f64,
    ) -> Self {
        let duration = Some(duration_secs);
        let metadata = ToolRunMetadata {
            duration_secs: duration,
            ..ToolRunMetadata::default()
        };
        Self {
            status: ToolStatus::Success,
            summary: summary.into(),
            model_content: model_content.into(),
            error: None,
            metadata: Box::new(metadata),
            artifacts: Vec::new(),
            duration_secs: duration,
        }
    }

    pub fn error(error: impl Into<String>, duration_secs: f64) -> Self {
        let error = error.into();
        let duration = Some(duration_secs);
        Self {
            status: ToolStatus::Error,
            summary: error.clone(),
            model_content: format!("Error: {}", error),
            error: Some(error),
            metadata: Box::new(ToolRunMetadata {
                duration_secs: duration,
                ..ToolRunMetadata::default()
            }),
            artifacts: Vec::new(),
            duration_secs: duration,
        }
    }

    pub fn cancelled() -> Self {
        Self {
            status: ToolStatus::Cancelled,
            summary: "[cancelled]".to_string(),
            model_content: "[Tool call skipped: the user cancelled before execution]".to_string(),
            error: None,
            metadata: Box::new(ToolRunMetadata::default()),
            artifacts: Vec::new(),
            duration_secs: None,
        }
    }

    pub fn with_metadata(mut self, mut metadata: ToolRunMetadata) -> Self {
        metadata.duration_secs = self.duration_secs;
        self.metadata = Box::new(metadata);
        self
    }

    pub fn with_artifacts(mut self, artifacts: Vec<ToolArtifact>) -> Self {
        self.artifacts = artifacts.clone();
        self.metadata.artifacts = artifacts;
        self
    }

    pub fn with_images(self, images: Vec<String>) -> Self {
        self.with_artifacts(
            images
                .into_iter()
                .map(|data| ToolArtifact::Image { data })
                .collect(),
        )
    }

    pub fn was_cancelled(&self) -> bool {
        self.status == ToolStatus::Cancelled
    }

    pub fn is_success(&self) -> bool {
        self.status == ToolStatus::Success
    }

    pub fn output(&self) -> &str {
        &self.model_content
    }

    pub fn error_message(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn images(&self) -> Option<Vec<String>> {
        let images: Vec<String> = self
            .artifacts
            .iter()
            .filter_map(|artifact| match artifact {
                ToolArtifact::Image { data } => Some(data.clone()),
                _ => None,
            })
            .collect();
        if images.is_empty() {
            None
        } else {
            Some(images)
        }
    }

    /// Convert to a textual representation suitable for embedding in
    /// the follow-up `tool` role message. Cancellation produces a
    /// placeholder so the model sees "this was skipped" rather than
    /// the history becoming malformed.
    pub fn as_tool_message_content(&self) -> String {
        self.model_content.clone()
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
    /// `StreamDone`. FIFO order. Each entry carries the attachment
    /// ids that were present when the user submitted it, so the
    /// auto-submit sends the images that belonged to *that* message.
    pub queued_messages: VecDeque<QueuedMessage>,
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
    /// Running accumulator for mouse-wheel scroll events (F13). The
    /// reducer adds the delta here on `Msg::MouseScroll`; the render
    /// layer compares against its last-seen snapshot and applies the
    /// diff to the chat pane's `ChatState`. This keeps the reducer
    /// pure — it doesn't touch render-layer state, it just publishes
    /// an intent. `i32` wraps at ~2 billion scrolls (never).
    pub mouse_scroll_accum: i32,
    /// Whether committed reasoning/thinking blocks are expanded in
    /// the chat transcript. Hidden by default to keep the TUI focused
    /// on user-facing work while retaining provider-required history.
    pub show_reasoning: bool,
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

/// A user message queued while a turn was in flight, with the attachment
/// ids that were present at submit time. Capturing the ids here (instead
/// of re-reading live `ui.attachments` at drain time) ensures the
/// auto-submit consumes the images the user attached to *this* message.
#[derive(Debug, Clone)]
pub struct QueuedMessage {
    pub text: String,
    pub attachment_ids: Vec<u64>,
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
}

/// One tool action awaiting inline approval. Built by the policy gate and
/// delivered via `Msg::ApprovalRequested`; rendered as a modal. The `prompt`
/// body is pre-formatted by the gate (command / path / summary, plus any
/// Auto-review reason) so the render layer stays dumb.
#[derive(Debug, Clone)]
pub struct PendingApproval {
    pub turn: TurnId,
    pub call_id: ToolCallId,
    pub tool: String,
    /// `RiskClass::as_str()` — shown on the title line.
    pub risk: String,
    pub kind: ApprovalKind,
    /// Pre-formatted body (the command/path being run + any classifier reason).
    pub prompt: String,
    /// What "don't ask again" (option 2) will allowlist, shown in the prompt.
    pub allowlist_scope: String,
    /// Highlighted option for arrow-key navigation: 0 = Yes, 1 = Yes-always,
    /// 2 = No. Number keys (1/2/3) still resolve directly regardless of this.
    pub selected_option: usize,
}

/// The user's answer to an approval prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalChoice {
    Approve,
    ApproveAlways,
    Deny,
}

/// Category of the gated action — drives the prompt's label. Mirrors the
/// runtime `ToolCategory` but lives in `domain` so the pure reducer needn't
/// depend on `providers`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalKind {
    Shell,
    FileMutation,
    Web,
    Mcp,
    Subagent,
    ComputerUse,
    Classify,
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
        let o = ToolOutcome::cancelled();
        assert!(o.was_cancelled());
        let content = o.as_tool_message_content();
        assert!(content.contains("cancelled"));
    }

    #[test]
    fn tool_outcome_finished_returns_output_verbatim() {
        let o = ToolOutcome::success("hello world", "hello world", 0.1);
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
