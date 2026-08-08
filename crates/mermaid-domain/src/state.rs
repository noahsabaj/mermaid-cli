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

use crate::ConversationHistory;
use crate::LoadedInstructions;
use crate::{Config, McpServerConfig};
use mermaid_model::models::ChatMessage;
use mermaid_model::models::tool_call::ToolCall as ModelToolCall;
use mermaid_model::models::{ProviderContinuation, ReasoningLevel, TokenUsage, TokenUsageSource};
use mermaid_runtime::SafetyMode;

use super::cmd::ChatRequest;
use super::compaction::CompactionTrigger;
use super::msg::Msg;
use super::runtime::RuntimeState;
use mermaid_model::ids::{IdAllocator, ToolCallId, TurnId};
use mermaid_model::question::PendingQuestionSet;
use mermaid_model::tool_run::{ToolArtifact, ToolRunMetadata, ToolStatus};

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
    pub memory: Option<crate::LoadedMemory>,
    /// Discovered SKILL.md playbooks (project/user/plugin) plus the rendered
    /// index injected into the model prompt alongside instructions and memory.
    /// Loaded once at startup — skills are authored artifacts, not live state.
    pub skills: Option<crate::LoadedSkills>,
    /// Context strings injected by `before_tool_use` plugin hooks
    /// (`additionalContext`), buffered until the next dispatched model
    /// request consumes them (see `push_call_model`). Byte-capped; transient
    /// (never persisted with the session).
    pub pending_hook_context: Vec<String>,
    /// One-line notices about the task checklist for the model's next
    /// request: user `/todos` edits, vetoed completions, staleness nudges.
    /// Same lifecycle as `pending_hook_context` (consumed by the next real
    /// dispatch, transient, never persisted).
    pub pending_task_notices: Vec<String>,
    /// Current working directory. Captured once at startup; tools
    /// receive it via `ExecContext::workdir` and spawned subprocesses
    /// inherit it. Centralized here so tests can inject a fake cwd.
    pub cwd: PathBuf,
    /// System temp dir, injected once at startup by the shell (which reads
    /// `std::env::temp_dir()`). Pasted-image attachments build their scratch
    /// path from it; holding it here keeps the reducer free of the env read it
    /// used to do inline (#54), and injecting it keeps the read out of this
    /// crate entirely.
    pub temp_dir: PathBuf,
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
    /// FIFO queue of `ask_user_question` batches awaiting the user's answers.
    /// The front item renders as a selectable modal; submitting pops it and
    /// emits `Cmd::ResolveQuestion`, unblocking the parked tool task. Empty in
    /// headless mode (no broker → the tool proceeds without asking).
    pub pending_question: VecDeque<PendingQuestionSet>,
    /// Runtime-only observability state: process registry, provider
    /// capability snapshot, and lifecycle timeline. Not sent to the
    /// model.
    pub runtime: RuntimeState,
    /// Quit flag. When set, the main loop drains pending effects and
    /// exits. The reducer never panics on its own; it sets this instead.
    pub should_exit: bool,
    /// Prompt-backed slash commands contributed by enabled plugins
    /// (`manifest.prompts`). Loaded once at startup by the run loop (like
    /// `skills`); the reducer expands `/name args` into a normal
    /// `Msg::SubmitPrompt`, so recordings replay without the plugin
    /// installed. Sorted by name.
    pub plugin_commands: Vec<PluginCommand>,
    /// `mermaid run --output-schema`: set by the headless driver before the
    /// dedicated formatting turn; `build_chat_request` copies it onto the
    /// request (dropping all tools for that turn). Never set interactively.
    pub output_schema: Option<serde_json::Value>,
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
    ///
    /// Pure given its inputs: `now` seeds the injected clock and derives the
    /// initial conversation's id/title, so `--replay` reconstructs the same
    /// starting state from a recorded header. Nothing here reads the
    /// environment, the filesystem, or the clock.
    ///
    /// `temp_dir` is last rather than beside `cwd` on purpose: two adjacent
    /// `PathBuf` parameters are silently swappable, and nothing in the type
    /// system would catch it.
    pub fn new(
        settings: Config,
        cwd: PathBuf,
        model_id: String,
        now: DateTime<Local>,
        temp_dir: PathBuf,
    ) -> Self {
        let project_path = cwd.display().to_string();
        let conversation = ConversationHistory::new(project_path, model_id.clone(), now);
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
                last_token_usage: None,
                cumulative_token_usage: TokenUsageTotals::default(),
                context_usage: None,
                is_subagent: false,
                agent_preamble: None,
                plan: None,
                // Materialized by the effect layer after startup dispatches
                // `Cmd::EnsureScratchpad`; the pure constructor never touches
                // the filesystem.
                scratchpad: None,
            },
            turn: TurnState::Idle,
            ui: UiState {
                last_title_dispatched: Some(initial_title),
                theme: settings.ui.theme,
                ..UiState::default()
            },
            mcp,
            settings,
            instructions: None,
            memory: None,
            skills: None,
            pending_hook_context: Vec::new(),
            pending_task_notices: Vec::new(),
            cwd,
            temp_dir,
            ids: IdAllocatorBundle::default(),
            confirm: None,
            pending_approval: VecDeque::new(),
            pending_question: VecDeque::new(),
            runtime,
            should_exit: false,
            output_schema: None,
            plugin_commands: Vec::new(),
            // Seed the injected clock from the caller (live: startup wall
            // clock; replay: the recorded header's ts). The driver overwrites
            // this on every iteration (Cause 3); the reducer never reads the
            // wall clock directly.
            now,
        }
    }

    /// Apply a `--continue` / `--sessions` seed: replace the fresh
    /// conversation with the loaded history and re-dispatch the terminal
    /// title once. Shared by the live driver and `--replay` so both
    /// construct the same starting state by definition.
    pub fn seed_conversation(&mut self, history: ConversationHistory) {
        let title = history.title.clone();
        // Restore the live meters + safety mode that ride on the saved file
        // (see `Session::snapshot_conversation`). Sessions saved before these
        // fields existed leave them at None/0, so keep the config-default
        // safety mode (already set by `State::new`) when the file has none.
        if let Some(mode) = history.safety_mode {
            self.session.safety_mode = mode;
        }
        // Restore planning-in-progress (None for sessions saved before the
        // field existed, and for sessions that weren't planning).
        self.session.plan = history.plan.clone();
        self.session.last_token_usage = history.last_token_usage;
        self.session.cumulative_token_usage = history.cumulative_token_usage;
        self.session.context_usage = history.context_usage.clone();
        self.session.conversation = history;
        // A session persisted mid-tool (an assistant `tool_use` with no committed
        // result, or a result whose call was archived out) would otherwise resume
        // with an orphan and 400 the first request. Repair pairing on the loaded
        // prefix so both the transcript and the next request are valid.
        crate::compaction::normalize_history(self.session.conversation.messages_mut());
        // Checklist retirement deliberately does NOT happen here. There is one
        // retirement rule and it lives at natural run end
        // (`handle_stream_done`), where the summary line absorbs the count so
        // retirement reads as completion rather than data loss.
        //
        // Retiring again at seed time made a SECOND rule with different
        // behavior: run end preserves the list when a run is cancelled or
        // errors, but a seed-time clear discarded any all-done list on the
        // next `--resume`/`--continue` — including one the user cancelled and
        // came back to. The transcript is not a substitute for the checklist
        // the next run resumes against.
        // Continue global image numbering past the highest number already in the
        // loaded transcript, so `[Image #16]` keeps referring to that same image
        // across --resume/--continue. Sessions saved before image numbering (no
        // `image_numbers`) yield max 0 → start at 1, the default. Shared live +
        // --replay seed path, so both reconstruct an identical allocator.
        let max_image = self
            .session
            .conversation
            .messages()
            .iter()
            .filter_map(|m| m.image_numbers.as_ref())
            .flatten()
            .copied()
            .max()
            .unwrap_or(0);
        self.ids.image = mermaid_model::ids::IdAllocator::starting_at(max_image + 1);
        self.ui.last_title_dispatched = Some(title);
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

/// Per-component token counts accumulated for UI display. Components
/// are disjoint (mirrors `TokenUsage`); totals are derived, never
/// stored. Providers report usage per API request; the session keeps
/// both the last request and the cumulative API usage so the footer
/// does not imply this is the current model context length.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TokenUsageTotals {
    pub prompt_tokens: usize,
    pub completion_tokens: usize,
    pub cached_input_tokens: usize,
    pub cache_creation_input_tokens: usize,
    pub reasoning_output_tokens: usize,
}

impl TokenUsageTotals {
    pub fn from_usage(usage: &TokenUsage) -> Self {
        Self {
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
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

    pub fn total_tokens(&self) -> usize {
        self.input_total_tokens()
            .saturating_add(self.output_total_tokens())
    }
}

/// Approximate request-context breakdown used before provider usage
/// arrives. These numbers are diagnostic estimates, not billing facts.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
        // input + output ≈ what the next request's prompt will occupy;
        // derived from disjoint components so it means the same thing
        // for every provider.
        Self::new(
            usage.total_tokens(),
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

    #[expect(clippy::too_many_arguments)]
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

/// The plan's DATA while `Session.safety_mode == SafetyMode::Plan` — never the
/// fact of being in plan mode, which the mode value alone decides. Plan IS a
/// safety mode (the strictest position in the Shift+Tab cycle), so there is no
/// second flag and no remembered restore target here; the policy gate applies
/// the plan carve-outs (the plan file itself, memory writes, known-safe builds)
/// off the mode.
///
/// Serialized into `ConversationHistory` on every save (like `safety_mode`)
/// so `--resume` restores planning-in-progress; sessions saved before this
/// field existed deserialize to `None`.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PlanState {
    /// Absolute path of the plan file the model authors — the single path the
    /// policy gate exempts from the read-only floor.
    pub plan_path: std::path::PathBuf,
    /// Model to restore when plan mode ends. `Some` only when `[plan] model`
    /// swapped the session onto a plan-phase model at entry.
    #[serde(default)]
    pub prev_model_id: Option<String>,
    /// Reasoning level to restore when plan mode ends. `Some` only when
    /// `[plan] reasoning` overrode it at entry.
    #[serde(default)]
    pub prev_reasoning: Option<mermaid_model::models::ReasoningLevel>,
}

/// The mode-defining facts the model was last told about, snapshotted at
/// each dispatch by the context-delta injector
/// (`reducer::advertise_context_changes`): the reducer diffs live state
/// against this and injects one persistent history marker per change, then
/// re-stamps it. One un-bypassable announcement path for plan entry/exit,
/// safety-mode flips, and model swaps — transitions themselves stay
/// message-log-free (the codex snapshot+diff pattern).
///
/// Lives on `ConversationHistory` (persisted with the transcript) so a
/// resumed session diffs against what THAT conversation's model last saw,
/// and `/clear`/fresh forks start from `None` (= seed silently, announce
/// nothing). Plan permissions stay out: a `/plan config` retune is already
/// reflected live in the system prompt and never contradicts history.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AdvertisedContext {
    /// `Some(plan_path)` while the model has been told it is planning.
    pub plan_path: Option<std::path::PathBuf>,
    pub safety_mode: SafetyMode,
    pub model_id: String,
}

impl AdvertisedContext {
    /// The facts as they stand right now — the injector's diff input.
    pub fn observe(session: &Session) -> Self {
        Self {
            plan_path: session.plan.as_ref().map(|p| p.plan_path.clone()),
            safety_mode: session.safety_mode,
            model_id: session.model_id.clone(),
        }
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
    /// Live safety mode for this session. Initialized from
    /// `config.safety.mode`, then mutated in-session by `Shift+Tab` /
    /// `/safety` (session-scoped — never written back to the config file).
    /// The reducer threads this into `Cmd::ExecuteTool` so the policy gate
    /// enforces the *current* mode, not the startup snapshot.
    pub safety_mode: SafetyMode,
    /// Token usage for the most recent completed provider request.
    /// `None` means the provider did not report usage for that turn.
    pub last_token_usage: Option<TokenUsageTotals>,
    /// Prompt/completion/total API usage accumulated for this session.
    pub cumulative_token_usage: TokenUsageTotals,
    /// Latest model-visible context snapshot. This may be an estimate
    /// while a request is in flight and is replaced by provider-reported
    /// usage when available.
    pub context_usage: Option<ContextUsageSnapshot>,
    /// True when this session IS a subagent (a child reducer driven by
    /// `SubagentTool`). `system_prompt_for_state` appends the subagent
    /// contract (final message = the report returned to the parent) when
    /// set. Never true for a user-facing session.
    pub is_subagent: bool,
    /// Agent-type system-prompt block (e.g. the Explore type's "read-only
    /// reconnaissance" charter), appended after the subagent contract.
    /// Only ever `Some` on subagent sessions.
    pub agent_preamble: Option<String>,
    /// `Some` while the session is in plan mode (see [`PlanState`]). Never
    /// `Some` on subagent sessions — children explore, they don't plan.
    pub plan: Option<PlanState>,
    /// Per-session scratch directory, once the effect layer has materialized
    /// it on disk (`Cmd::EnsureScratchpad` -> `Msg::ScratchpadReady`). `None`
    /// until then, and reset whenever the conversation id changes (`/clear`,
    /// `/load`, rewind fork) — the reducer re-emits `EnsureScratchpad` at
    /// those points. The reducer stamps this onto `Cmd::ExecuteTool` so tools
    /// see it via `ExecContext::scratchpad`. Runtime-only, never persisted.
    pub scratchpad: Option<PathBuf>,
}

impl Session {
    /// Clone the conversation with the live meters + safety mode overlaid, so
    /// a saved file carries the full restorable state. These fields live on
    /// `Session` (which is NOT serialized — only `conversation` is), so every
    /// `Cmd::SaveConversation` snapshots them in and `seed_conversation`
    /// hydrates them back on resume.
    pub fn snapshot_conversation(&self) -> ConversationHistory {
        let mut history = self.conversation.clone();
        history.safety_mode = Some(self.safety_mode);
        history.plan = self.plan.clone();
        history.last_token_usage = self.last_token_usage;
        history.cumulative_token_usage = self.cumulative_token_usage;
        history.context_usage = self.context_usage.clone();
        history
    }

    /// The committed message log. All messages visible in the chat
    /// widget live here; partial in-flight content lives in
    /// `TurnState::Generating`.
    pub fn messages(&self) -> &[ChatMessage] {
        self.conversation.messages()
    }

    /// Append a committed assistant/user/tool message. Mutation happens
    /// through here so the reducer has one chokepoint to update the
    /// conversation's `updated_at` and derived title.
    ///
    /// `now` is the reducer's injected clock (`state.now`). It stamps both
    /// the message's commit timestamp and `updated_at` — the wall-clock
    /// stamp `ChatMessage::new` put on the message at construction is
    /// deliberately overwritten with the deterministic one, so `update()`
    /// is a pure function and `--replay` recommits identical messages.
    pub fn append(&mut self, mut msg: ChatMessage, now: DateTime<Local>) {
        msg.timestamp = now;
        self.conversation.add_messages(&[msg], now);
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
        /// Opaque provider state carried until the assistant message commits.
        provider_continuation: Option<ProviderContinuation>,
        /// Tool calls the model has streamed so far this turn.
        /// `StreamToolCall` messages push here; `StreamDone` drains
        /// the vec, allocates `PendingToolCall` entries, and
        /// transitions to `ExecutingTools`. When the vec is empty at
        /// stream end, the turn returns to `Idle`.
        pending_tool_calls: Vec<ModelToolCall>,
        /// True when this turn resumes a reply cut by the per-response
        /// output cap (auto-continue). The commit stamps the resulting
        /// message `ChatMessageKind::Continuation` so the transcript can
        /// stitch it into the previous bubble. Survives an intervening
        /// empty-retry or truncation-recovery compaction so a chain never
        /// loses the marker mid-way.
        continuation: bool,
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
    /// Summarizing history as a step of its own: a manual `/compact`
    /// (`trigger: Manual`, ends the turn afterwards) or a truncation recovery
    /// (`trigger: TruncationRecovery`, resumes the run afterwards). Pre-turn auto
    /// compaction instead runs while `Generating` because it is preflight for the
    /// same user turn. `trigger` is what the finished/failed handlers key off.
    Compacting {
        id: TurnId,
        started: SystemTime,
        trigger: CompactionTrigger,
        /// True when the turn that led into this compaction was itself a
        /// continuation (see `Generating::continuation`): a `TruncationRecovery`
        /// resume must re-enter `Generating` with the flag intact or a
        /// continuation chain interrupted by a genuine context-full compaction
        /// would commit its remaining text unmarked.
        resume_continuation: bool,
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
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
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

    /// Override the status after construction. When transitioning to
    /// `Error`, populate `error` from `model_content` (if not already set)
    /// so the renderer — `action_display_for`, which falls back to
    /// `error_message().unwrap_or("[cancelled]")` — surfaces the failure
    /// instead of mislabeling it as a cancellation. The MCP proxy uses this
    /// for `isError: true` results (#91): the model still sees the server's
    /// content verbatim via `model_content`, but the outcome reads as an
    /// error rather than a success.
    pub fn with_status(mut self, status: ToolStatus) -> Self {
        if status == ToolStatus::Error && self.error.is_none() {
            self.error = Some(self.model_content.clone());
        }
        self.status = status;
        self
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

/// Live activity for one in-flight tool call (today: a subagent child).
/// `activity` is a short stable label ("`read_file`…", "thinking"); `tokens`
/// is the child's cumulative output-token estimate, throttled at the source.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LiveToolStatus {
    pub activity: String,
    pub tokens: usize,
}

/// One plugin-contributed slash command (a markdown prompt from an enabled
/// plugin's `manifest.prompts`). Plain data — parsing/IO happens in
/// `app::plugin_assets`; the reducer only expands and submits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginCommand {
    /// Command name without the leading `/` (validated `[a-z0-9-]+`).
    pub name: String,
    /// One-line description for the palette and `/help`.
    pub description: String,
    /// The prompt body. `$ARGUMENTS` is replaced with the typed args;
    /// without the token, non-empty args append as a final paragraph.
    pub body: String,
    /// Owning plugin name, shown as `(plugin:<name>)` in the palette.
    pub plugin: String,
}

impl PluginCommand {
    /// Expand the body with typed arguments: replace-all of `$ARGUMENTS`
    /// when the token is present, else append the args as a new paragraph
    /// when non-empty. Pure.
    pub fn expand(&self, args: &str) -> String {
        let args = args.trim();
        if self.body.contains("$ARGUMENTS") {
            return self.body.replace("$ARGUMENTS", args);
        }
        if args.is_empty() {
            self.body.clone()
        } else {
            format!("{}\n\n{}", self.body, args)
        }
    }
}

/// All UI-only state. Things in `UiState` never affect what gets sent
/// to the model — only what the user sees.
#[derive(Debug, Clone, Default)]
pub struct UiState {
    pub mode: UiMode,
    /// Active color theme. Seeded from `config.ui.theme` in `State::new`;
    /// `/theme` switches it live (and persists via `Cmd::PersistUiTheme`).
    /// The render layer memoizes the resolved `Theme` off this value.
    pub theme: crate::ThemeChoice,
    /// `NO_COLOR` was set (present and non-empty) at startup. Injected by the
    /// run loop after `State::new` — the reducer never reads the environment.
    /// While true the render layer draws `Theme::plain()` regardless of
    /// `theme`, and `/theme` notes that colors are disabled.
    pub no_color: bool,
    pub input_buffer: String,
    /// Byte position within `input_buffer`. The reducer normalizes to
    /// a UTF-8 char boundary on every mutation via
    /// `floor_char_boundary`, so widgets can slice safely.
    pub input_cursor: usize,
    /// Pending image pastes for the next user message. Each is mirrored by an
    /// inline `[Image #N]` token in `input_buffer`; the token is the source of
    /// truth at submit time (see `image_token` + `handle_submit_prompt`).
    pub attachments: Vec<Attachment>,
    /// In-flight `Cmd::ReadClipboard` reads (Ctrl+V) whose result
    /// (`Msg::ClipboardRead`) hasn't arrived yet. A counter, not a bool, so a
    /// burst of rapid Ctrl+V presses all drain before a held submit fires.
    /// Incremented where `Cmd::ReadClipboard` is pushed; decremented in
    /// `handle_clipboard_read`.
    pub clipboard_reads_pending: u32,
    /// Set when Enter is pressed while `clipboard_reads_pending > 0`: the submit
    /// is held until the read drains so a fast paste-then-Enter still includes
    /// the pasted image instead of racing past it. `handle_clipboard_read`
    /// re-runs the submit once the last pending read lands.
    pub submit_after_clipboard: bool,
    /// When `Some(i)`, the palette has a highlighted row. `None` =
    /// closed / not showing.
    pub palette_cursor: Option<usize>,
    /// Cached project file list for the @-mention picker (relative paths,
    /// dirs with a trailing `/`). `None` until the first walk completes;
    /// stale-while-revalidate — every picker OPEN refreshes it.
    pub project_files: Option<Vec<String>>,
    /// A `Cmd::ListProjectFiles` walk is in flight (dedupe: opening the
    /// picker again while loading must not spawn a second walk).
    pub project_files_loading: bool,
    /// Current fuzzy matches for the active @-token, best first (top 50).
    /// Recomputed in the reducer on every text mutation — not per-frame in
    /// render — because fuzzy-ranking 20k paths at 60 Hz would be wasteful.
    pub file_picker_matches: Vec<String>,
    /// Highlighted row in `file_picker_matches`. `None` = picker closed.
    pub file_picker_cursor: Option<usize>,
    /// The user Esc'd the picker for the CURRENT token; cleared on the next
    /// text mutation so typing reopens it.
    pub file_picker_dismissed: bool,
    /// Messages the user typed while a turn was in flight, FIFO. Mid-run
    /// steering drains the WHOLE queue at each tool boundary (committed as
    /// user messages before the follow-up model call); a message queued
    /// mid-stream with no later tool boundary drains one-at-a-time at turn
    /// end instead. Each entry carries the attachment ids that were present
    /// when the user submitted it, so delivery sends the images that
    /// belonged to *that* message.
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
    /// Live activity per in-flight tool call, keyed by the call id.
    /// Fed by `Msg::ToolProgress` (today: subagent activity — the child's
    /// current tool / coarse phase plus a throttled token count) and rendered
    /// by the agent panel + status line next to the tool label. Entries are
    /// removed on that call's `ToolFinished` and the map is cleared when the
    /// turn ends or cancels; call ids are session-unique, so a stale entry
    /// can never attach to a later call.
    pub live_tool_status: HashMap<ToolCallId, LiveToolStatus>,
    /// Up-arrow history navigation cursor into
    /// `session.conversation.input_history`. `None` = not
    /// navigating (`input_buffer` is whatever the user typed).
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
    /// Monotonic "jump to bottom" counter (keyboard `End`). Same
    /// publish-then-diff pattern as `mouse_scroll_accum`: the reducer bumps it,
    /// the render layer diffs it against its last-seen value and calls
    /// `ChatState::resume_auto_scroll` — keeping the reducer pure.
    pub scroll_to_bottom_seq: u32,
    /// Monotonic "repaint everything" counter. Same publish-then-diff pattern
    /// as `scroll_to_bottom_seq`: the reducer bumps it, the run loop diffs it
    /// against its last-seen value and calls `Terminal::clear()` before the
    /// next draw. Needed because ratatui diff-renders against its back buffer:
    /// bytes some OTHER process wrote to the tty (a child that opened
    /// `/dev/tty`, a stray `printf` from another terminal) are invisible to
    /// that buffer and would otherwise persist as ghost cells. Bumped when a
    /// shell command finishes and on Ctrl+L.
    pub full_redraw_seq: u32,
    /// Ctrl+C exit arming (press-twice-to-exit). `Some(deadline)` after a
    /// first Ctrl+C: a second press at or before the deadline exits; any
    /// other key disarms; past the deadline the next Ctrl+C re-arms. Expiry
    /// is lazy — compared against `state.now`, so the render hint vanishes on
    /// the next tick with no state change. Ctrl+D on empty input and `/quit`
    /// still exit immediately.
    pub exit_armed_until: Option<DateTime<Local>>,
    /// Double-Esc rewind arming. `Some(t)` after an idle Esc; a second Esc
    /// within `ESC_REWIND_WINDOW_MS` of `t` opens the rewind picker. Any
    /// other key disarms; expiry is lazy against `state.now` like
    /// `exit_armed_until` (the hint vanishes on the next tick). Busy Esc
    /// never arms — it stays the cancel gesture.
    pub esc_armed_at: Option<DateTime<Local>>,
    /// Whether the terminal window has LOST focus (from terminal focus
    /// reporting via `Msg::FocusChanged`). Defaults `false` (assume attended, so
    /// terminals without focus reporting never ding); the attention bell fires
    /// only while this is `true`.
    pub terminal_unfocused: bool,
    /// Whether committed reasoning/thinking blocks are expanded in
    /// the chat transcript. Hidden by default to keep the TUI focused
    /// on user-facing work while retaining provider-required history.
    pub show_reasoning: bool,
    /// Whether the task checklist under the status line is collapsed to its
    /// one-line form (Ctrl+T toggles). Named for the non-default state so
    /// `derive(Default)` yields expanded, session-scoped, never persisted.
    pub tasks_collapsed: bool,
    /// Ephemeral confirmation for an action the user just took by hand
    /// ("copied 42 chars to clipboard"): the text and the instant it stops
    /// being drawn. Expiry is lazy against `state.now` — the 60 Hz tick makes
    /// it vanish on its own, exactly like `esc_armed_at`.
    ///
    /// Deliberately NOT the transcript. A copy confirmation is feedback on a
    /// keystroke, not part of the conversation; parking it in the message log
    /// left a permanent "Copied N chars to clipboard" row above the input for
    /// the rest of the session. Anything worth KEEPING (an error, a config
    /// change) still goes through `Msg::TransientStatus` to the transcript.
    pub toast: Option<(String, DateTime<Local>)>,
}

/// How long a [`UiState::toast`] stays on screen.
pub const TOAST_TTL: chrono::Duration = chrono::Duration::milliseconds(2000);

impl UiState {
    /// The @-mention token under the cursor, when the picker may show:
    /// not user-dismissed, and not while the buffer is a slash command
    /// (the slash palette owns that surface).
    pub fn active_file_token(&self) -> Option<crate::file_mention::AtToken> {
        if self.file_picker_dismissed || self.input_buffer.starts_with('/') {
            return None;
        }
        crate::file_mention::active_at_token(&self.input_buffer, self.input_cursor)
    }

    /// Whether the @-mention file picker is currently open.
    pub fn file_picker_open(&self) -> bool {
        self.active_file_token().is_some()
    }
}

/// One selectable row in the `/model` picker.
///
/// Deliberately flat data, not a provider handle: discovery runs in the effect
/// layer and hands the reducer plain strings, so the picker renders (and
/// `--replay` reproduces) without touching the network.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ModelChoice {
    /// The full id `/model <id>` would take — `ollama/llama3.2`,
    /// `anthropic/claude-opus-4-5`.
    pub id: String,
    /// Group heading this row sits under: `Local (Ollama)`, `anthropic`, …
    pub group: String,
    /// Dim right-hand column: what the row is good for, or why it can't run.
    pub detail: String,
    /// Ready to use right now. `false` for an Ollama model that is known but
    /// not pulled — still selectable (selection triggers the pull), but marked
    /// so the list never implies it will answer instantly.
    pub ready: bool,
}

/// Top-level UI mode. Like `TurnState` this is a sum type instead of a
/// zoo of independent bools. `EditingInput` is the default.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum UiMode {
    #[default]
    EditingInput,
    /// `/load` — list of saved conversations visible. `candidates`
    /// holds what the effect handler returned; `cursor` is the
    /// highlighted row.
    ConversationList {
        candidates: Vec<ConversationSummary>,
        cursor: usize,
    },
    /// `/model` — list of available models visible.
    ModelList,
    /// `/model` with no argument: the interactive model picker. `candidates`
    /// is everything discovery found (local Ollama models plus each keyed
    /// remote provider's catalog); `query` narrows it as the user types, which
    /// is what keeps a provider returning 200 ids usable.
    ModelPicker {
        candidates: Vec<ModelChoice>,
        query: String,
        cursor: usize,
        /// Discovery is still in flight. Rendered as a "searching…" row rather
        /// than an empty list, so a slow provider doesn't look like "no models".
        loading: bool,
    },
    /// Double-Esc rewind: pick an earlier user message to fork the session
    /// at. Candidates are user-role Normal messages, newest first. Selecting
    /// one forks into a NEW session (original preserved, lineage stamped)
    /// with the composer pre-filled.
    /// The `/plan config` settings picker: per-category permission levels,
    /// model/reasoning overrides, approval behavior. `cursor` is the
    /// highlighted row.
    PlanConfig { cursor: usize },
    RewindPicker {
        candidates: Vec<RewindCandidate>,
        cursor: usize,
    },
}

/// One rewind target: a user message's position in the conversation plus a
/// one-line excerpt for the picker row. Never rides in a `Msg` (the whole
/// flow is Key-driven), so no serde — record/replay work unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewindCandidate {
    /// Index into `conversation.messages` of the user message; the fork
    /// keeps `messages[..index]` and pre-fills the composer with this one.
    pub message_index: usize,
    /// First line of the message, clipped for the picker row.
    pub excerpt: String,
}

/// Summary row for the conversation picker. Produced by
/// `Cmd::ListConversations` → `Msg::ConversationsListed`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
    /// Global, conversation-wide image number — the `N` shown in the inline
    /// `[Image #N]` token and, once sent, in the committed message. Stable for
    /// the life of the image; distinct from `id`, which only scopes attachment
    /// ownership within a submit.
    pub number: u64,
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
    /// Deferred MCP tools promoted to direct advertisement by a
    /// `tool_search` call this session (sanitized full names). A
    /// `BTreeSet` keeps the advertised tool order byte-stable across
    /// requests for prompt-cache warmth (#F68). Transient: cleared by
    /// conversation switch/`/clear` along with the rest of the session.
    pub promoted: std::collections::BTreeSet<String>,
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

/// Subset of the MCP `ToolDefinition` carried in reducer state. `name` is
/// the FULL sanitized advertised name (`mcp__<server>__<tool>`, provider-safe
/// charset and length — see `crate::mcp::sanitize`); `raw_name` is the bare
/// tool name exactly as the server advertised it, used for user-facing
/// display and for `enabled_tools`/`disabled_tools` filtering.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct McpToolSpec {
    pub name: String,
    /// Bare tool name as the server advertised it (pre-sanitization).
    #[serde(default)]
    pub raw_name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    /// Server-advertised `annotations.readOnlyHint` (UNTRUSTED; absent ⇒
    /// false = write-shaped). Feeds the external-writes policy floor: it can
    /// only keep a read at its old permissiveness, never grant more than the
    /// safety mode gives.
    #[serde(default)]
    pub read_only_hint: bool,
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

/// Category of the gated action — drives the prompt's label.
///
/// A deliberately coarser projection of `mermaid_runtime::ToolCategory`: seven
/// prompt labels for twelve policy categories, plus `Classify` which has no
/// `ToolCategory` at all. The mapping is the `From` impl below, exhaustive so a
/// new `ToolCategory` variant is a compile error in exactly one place.
///
/// (The previous comment here claimed the duplication existed "so the pure
/// reducer needn't depend on `providers`". That was false twice over:
/// `ToolCategory` lives in `mermaid-runtime`, which domain already depends on,
/// and the reducer did import from `providers` anyway.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ApprovalKind {
    Shell,
    FileMutation,
    Web,
    Mcp,
    Subagent,
    ComputerUse,
    Classify,
}

impl From<mermaid_runtime::ToolCategory> for ApprovalKind {
    fn from(category: mermaid_runtime::ToolCategory) -> Self {
        use mermaid_runtime::ToolCategory as C;
        match category {
            C::Edit => ApprovalKind::FileMutation,
            C::Shell | C::Git | C::Process => ApprovalKind::Shell,
            C::Web | C::Network | C::ExternalDirectory => ApprovalKind::Web,
            C::Mcp => ApprovalKind::Mcp,
            C::Subagent => ApprovalKind::Subagent,
            C::ComputerUse => ApprovalKind::ComputerUse,
            // `Read` and `Memory` resolve to Allow/Deny in `decide`, so neither
            // reaches an approval prompt; the arm exists to keep the match
            // total. The label is a poor fit and would read wrong if one ever
            // did reach a prompt -- worth revisiting, but not in a move.
            C::Read | C::Memory => ApprovalKind::Shell,
        }
    }
}

/// Severity carried on `Msg::CompactionFailed`. The compaction-failed handler
/// uses it to distinguish a benign no-op (`Info`, e.g. too little history to
/// compact) from a real failure worth surfacing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum StatusKind {
    Info,
    Warn,
    Error,
}

/// All ID allocators for the session. Grouped so the reducer can
/// request any of them through a single `&mut state.ids`.
#[derive(Debug, Clone, Copy, Default)]
pub struct IdAllocatorBundle {
    pub turn: IdAllocator,
    pub tool_call: IdAllocator,
    /// Global, conversation-wide image counter. Every pasted image draws its
    /// stable `[Image #N]` display number from here, so the number stays with
    /// that image across the whole chat (and across `--resume`, which reseeds it
    /// past the highest persisted number in `seed_conversation`).
    pub image: IdAllocator,
}

impl IdAllocatorBundle {
    pub fn fresh_turn(&mut self) -> TurnId {
        TurnId(self.turn.next())
    }

    pub fn fresh_tool_call(&mut self) -> ToolCallId {
        ToolCallId(self.tool_call.next())
    }

    pub fn fresh_image(&mut self) -> u64 {
        self.image.next()
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
            chrono::Local::now(),
            PathBuf::from("/tmp"),
        )
    }

    #[test]
    fn fresh_state_is_idle() {
        let s = mock_state();
        assert!(matches!(s.turn, TurnState::Idle));
        assert!(!s.is_busy());
        assert!(s.current_turn_id().is_none());
    }

    fn state_for(settings: Config, model_id: &str) -> State {
        State::new(
            settings,
            PathBuf::from("/tmp/project"),
            model_id.to_string(),
            chrono::Local::now(),
            PathBuf::from("/tmp"),
        )
    }

    /// The user's configured reasoning level must survive session bootstrap —
    /// without this it silently reverts to the enum default on every start.
    ///
    /// These assertions used to sit on `ModelConfig::from_app_config`, a
    /// byte-identical copy of this rule that nothing ever called. Deleting the
    /// duplicate left the live rule — right here in `State::new` — with no
    /// coverage at all, so the tests moved to it rather than going away.
    #[test]
    fn session_reasoning_comes_from_the_global_default() {
        let mut settings = Config::default();
        settings.default_model.reasoning = ReasoningLevel::High;
        let state = state_for(settings, "ollama/qwen3-coder:30b");
        assert_eq!(state.session.reasoning, ReasoningLevel::High);
        assert_eq!(state.session.model_id, "ollama/qwen3-coder:30b");
    }

    #[test]
    fn session_reasoning_falls_back_to_the_enum_default_when_unset() {
        let state = state_for(Config::default(), "ollama/qwen3-coder:30b");
        assert_eq!(
            state.session.reasoning,
            Config::default().default_model.reasoning
        );
    }

    /// A per-model preference beats the global default: `/reasoning high` on
    /// one model sticks for that model without touching any other.
    #[test]
    fn session_reasoning_prefers_the_per_model_entry() {
        let mut settings = Config::default();
        settings.default_model.reasoning = ReasoningLevel::Low;
        settings.reasoning_per_model.insert(
            "anthropic/claude-sonnet-4-6".to_string(),
            ReasoningLevel::High,
        );

        let pinned = state_for(settings.clone(), "anthropic/claude-sonnet-4-6");
        assert_eq!(pinned.session.reasoning, ReasoningLevel::High);

        let other = state_for(settings, "ollama/foo");
        assert_eq!(other.session.reasoning, ReasoningLevel::Low);
    }

    #[test]
    fn snapshot_and_seed_round_trip_restores_meters_and_safety() {
        // Move the live session state away from its `State::new` defaults, then
        // snapshot it into a conversation and seed it back into a fresh state —
        // this is exactly the save→resume path.
        let mut src = mock_state();
        src.session.safety_mode = SafetyMode::FullAccess;
        src.session.cumulative_token_usage = TokenUsageTotals {
            prompt_tokens: 4321,
            ..Default::default()
        };
        src.session.last_token_usage = Some(TokenUsageTotals {
            prompt_tokens: 100,
            ..Default::default()
        });
        src.session.context_usage = Some(ContextUsageSnapshot::new(
            8000,
            Some(128_000),
            TokenUsageSource::Estimate,
            8000,
            0,
            0,
            0,
            0,
            None,
        ));

        let snapshot = src.session.snapshot_conversation();

        let mut restored = mock_state();
        assert_eq!(
            restored.session.safety_mode,
            SafetyMode::Ask,
            "config default"
        );
        assert_eq!(restored.session.cumulative_token_usage.total_tokens(), 0);

        restored.seed_conversation(snapshot);
        assert_eq!(restored.session.safety_mode, SafetyMode::FullAccess);
        assert_eq!(restored.session.cumulative_token_usage.total_tokens(), 4321);
        assert_eq!(
            restored.session.last_token_usage.unwrap().total_tokens(),
            100
        );
        assert_eq!(restored.session.context_usage.unwrap().used_tokens, 8000);
    }

    #[test]
    fn seed_from_pre_persistence_file_keeps_config_default_safety() {
        // A conversation saved before these fields existed has `safety_mode:
        // None`; seeding it must NOT clobber the config-default mode that
        // `State::new` already set.
        let history = ConversationHistory::new(
            "/tmp/p".to_string(),
            "ollama/test".to_string(),
            chrono::Local::now(),
        );
        assert_eq!(history.safety_mode, None);
        let mut restored = mock_state();
        restored.session.safety_mode = SafetyMode::Auto; // stand in for a config default
        restored.seed_conversation(history);
        assert_eq!(
            restored.session.safety_mode,
            SafetyMode::Auto,
            "a None saved mode must not override the config default"
        );
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
            provider_continuation: None,
            pending_tool_calls: Vec::new(),
            continuation: false,
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
        s.session.append(ChatMessage::user("hi"), s.now);
        assert_eq!(s.session.messages().len(), 1);
        assert_eq!(s.session.messages()[0].content, "hi");
    }
}
