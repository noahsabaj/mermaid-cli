//! Everything the reducer asks the outside world to do.
//!
//! `Cmd` values are inert data structures. The reducer returns them
//! alongside each new `State`; `effect::EffectRunner::dispatch` then
//! turns them into real work (spawning tokio tasks, writing files,
//! hitting HTTP endpoints, killing processes). The reducer itself
//! never performs any I/O.
//!
//! This is the "effects as data" pattern from Elm/Redux. Three
//! payoffs this rewrite relies on:
//!
//!   1. **Testable reducer.** Assertions are `state, cmds = update(...)
//!      ; assert_eq!(cmds[0], Cmd::CallModel { … })`. No tokio, no
//!      mocks, no filesystem.
//!   2. **Uniform middleware.** Retry, tracing, rate-limiting wrap the
//!      dispatcher once instead of being re-implemented per adapter.
//!   3. **Replayable sessions.** `--record` dumps every `Msg`; the
//!      effects the reducer asked for are fully determined by the Msg
//!      log + initial state, so `--replay` is a pure fold.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::app::McpServerConfig;
use crate::session::ConversationHistory;
use mermaid_model::models::ChatMessage;
use mermaid_model::models::ReasoningLevel;
use mermaid_model::models::tool_call::ToolCall as ModelToolCall;
use mermaid_runtime::{SafetyMode, TaskStatus};

use super::state::ApprovalChoice;
use mermaid_model::question::QuestionResolution;

use super::compaction::{CompactionArchive, CompactionEvent, CompactionRequest};
use mermaid_model::ids::{ToolCallId, TurnId};
use mermaid_model::tool_run::ManagedProcess;

/// A single side-effect request. Most variants are one-shot; `CallModel`
/// and `ExecuteTool` spawn long-running tasks inside a per-turn
/// `TurnScope`.
// Several variants legitimately carry large payloads (a full
// `ConversationHistory` / `ChatRequest`). Boxing them would churn ~20
// construction + match sites for no real gain — `Cmd` values are short-lived
// and moved, not stored in bulk.
#[expect(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum Cmd {
    // ── Model + tool execution (the scope-spawning variants) ────────
    /// Dispatch the next chat request. Effect runner maps this onto
    /// `ModelProvider::chat` for the session's active provider.
    CallModel { turn: TurnId, request: ChatRequest },
    /// Generate a compact context checkpoint without continuing into
    /// a normal assistant turn.
    CompactConversation {
        turn: TurnId,
        request: CompactionRequest,
    },
    /// Run one tool in parallel with any other tools in the same turn.
    /// The runner wires `ExecContext::token` to the turn's scope so
    /// `Cmd::CancelScope` aborts them all at once.
    ///
    /// `model_id` is the active session's model id at the moment this
    /// tool call was emitted. The runner passes it into `ExecContext`
    /// so tools like `SubagentTool` can spawn children against the
    /// same provider the parent is using.
    ExecuteTool {
        turn: TurnId,
        call_id: ToolCallId,
        source: ModelToolCall,
        model_id: String,
        /// Effective live safety mode at the moment this call was emitted
        /// (`state.session.safety_mode`, floored to `ReadOnly` while a plan
        /// is being drafted). The runner builds the policy gate / Auto
        /// classifier from this rather than the static config.
        safety_mode: SafetyMode,
        /// `Some(path)` while the session is in plan mode: the one path the
        /// policy gate exempts from the read-only floor, and the flag the
        /// plan carve-outs (memory writes, known-safe builds) key on.
        plan_file: Option<std::path::PathBuf>,
        /// LIVE per-category plan permission levels (`/plan config` edits
        /// them mid-session; the startup `Config` snapshot in `ExecContext`
        /// would go stale). Only consulted while `plan_file` is `Some`.
        plan_permissions: crate::app::PlanPermissions,
        /// Context-window fill at dispatch, when known. `exit_plan_mode`
        /// shows it on the clear-context option so the tradeoff is legible.
        context_percent: Option<u8>,
        /// The user's stated intent for the turn (latest user message),
        /// passed to the Auto-mode classifier as alignment context.
        intent: Option<String>,
        /// Conversation id at dispatch — checkpoint-anchoring provenance
        /// (rides into `ExecContext` and onto any checkpoint this call takes).
        session_id: String,
        /// Conversation length (`messages().len()`) at dispatch. A fork at
        /// user-message index `k` discards checkpoints with index > k.
        message_index: usize,
        /// Per-session scratch directory (`Session::scratchpad`) at dispatch.
        /// `None` until `Msg::ScratchpadReady` lands — the runner threads it
        /// into `ExecContext::scratchpad` for tools to use as temp space.
        scratchpad: Option<PathBuf>,
    },
    /// Cancel every task in the given turn's `TurnScope`. After the
    /// scope's `JoinSet` drains (bounded by a ~2s timeout), the runner
    /// emits a single `Msg::TurnCancelled(turn)` — that, not
    /// `Msg::StreamDone`, is the terminal event that lets the reducer
    /// transition `Cancelling → Idle`. A same-id `Msg::StreamDone` that
    /// races the drain does NOT end the turn: `handle_stream_done` only
    /// acts on a `Generating` turn and restores the prior state
    /// (`Cancelling`) otherwise, so `TurnCancelled` stays the one
    /// terminal signal for a cancel.
    CancelScope(TurnId),
    /// Ctrl+B: signal the turn's scope to BACKGROUND (not cancel) its running
    /// work. The scope's background token fires; detachable tools (execute_
    /// command) move their child to a background process and return. The scope
    /// is left intact (unlike `CancelScope`).
    BackgroundScope(TurnId),

    /// Resolve an inline approval prompt: deliver the user's decision to the
    /// parked tool task via the `ApprovalBroker`. NOT turn-scoped — it's a
    /// fire-and-forget to the broker (the tool task it unblocks is the
    /// turn-scoped work).
    ResolveApproval {
        call_id: ToolCallId,
        decision: ApprovalChoice,
    },

    /// Resolve an `ask_user_question` prompt: deliver the user's answers to the
    /// parked tool task via the `QuestionBroker`. Like `ResolveApproval`, a
    /// fire-and-forget to the broker (not turn-scoped).
    ResolveQuestion {
        call_id: ToolCallId,
        resolution: QuestionResolution,
    },

    /// Overwrite the effect-side `TaskBroker` store. Emitted where the
    /// reducer changes checklist truth outside the broker's own publish
    /// cycle: rewind/fork and `/clear` (both clear it) and `--replay`
    /// re-seeding. Fire-and-forget to the broker, not turn-scoped.
    SyncTaskStore(crate::domain::checklist::ChecklistStore),
    /// Persist the `[plan]` table to the user config file (the `/plan
    /// config` picker edits live state; this writes it through the
    /// key-scoped updater so unrelated keys and defaults stay unfrozen).
    PersistPlanConfig(crate::app::PlanConfig),

    /// A user `/tasks` edit. Routed through the effect runner to the
    /// `TaskBroker` (the single writer) instead of mutating reducer state
    /// directly, so a concurrent tool call can't clobber it; the broker's
    /// `Msg::TasksUpdated` publish brings the result back.
    UserTaskEdit(crate::domain::checklist::UserChecklistEdit),

    /// A task transitioned to completed: run the gated `task_completed`
    /// plugin hook. A denying hook flips the task back to in_progress via
    /// the broker and queues a notice for the model's next turn.
    NotifyTaskCompleted {
        task: crate::domain::checklist::ChecklistItem,
        completed: u32,
        total: u32,
    },

    /// Materialize the per-session scratch directory for this conversation
    /// id (creating it under the private temp dir + stamping its pid lock),
    /// then report the path back via `Msg::ScratchpadReady`. Emitted at
    /// startup and whenever the conversation id changes (`/clear`, `/load`,
    /// rewind fork). The handler also opportunistically sweeps stale sibling
    /// scratchpads. Fire-and-forget, not turn-scoped.
    EnsureScratchpad { session_id: String },

    /// `/scratchpad` — list the session scratch directory's contents back
    /// into the transcript (bounded ASCII listing via `Msg::RuntimeText`).
    /// Only emitted while `Session::scratchpad` is stamped; carries the
    /// path so the effect never re-derives it. Fire-and-forget.
    ListScratchpad { path: PathBuf },

    // ── Persistence ─────────────────────────────────────────────────
    /// Save the current conversation to disk. No-op if unchanged since
    /// last save (effect-side idempotence).
    SaveConversation(ConversationHistory),
    /// Persist the raw messages removed by a compaction, then the compacted
    /// (message-stripped) conversation. Both are written by ONE effect task,
    /// archive first — only overwriting the conversation if the archive
    /// persisted — so a failed/lagging archive can never lose messages while
    /// the stripped conversation is saved over the old one.
    SaveCompactionArchive {
        archive: CompactionArchive,
        record: CompactionEvent,
        conversation: ConversationHistory,
    },
    /// Persist a daemon-visible background process record.
    SaveProcess(ManagedProcess),
    /// Persist the active model ID as `last_used_model`.
    PersistLastModel(String),
    /// Persist reasoning level tied to a specific model ID.
    PersistReasoningFor {
        model_id: String,
        level: ReasoningLevel,
    },
    /// Persist (or clear, when `num_ctx` is `None`) a per-model Ollama `num_ctx`
    /// override set via `/context <n>`/`max`/`auto`.
    PersistOllamaNumCtxFor {
        model_id: String,
        num_ctx: Option<u32>,
    },
    /// Persist the Ollama RAM-offload toggle (`/context offload on|off`).
    PersistOllamaOffload(bool),
    /// Persist the `/theme` choice as `ui.theme` in the user config file.
    PersistUiTheme(crate::app::ThemeChoice),
    /// List saved memories; emits `Msg::RuntimeText` with the rendered list.
    ListMemory,
    /// Save free-text to private memory; emits `Msg::MemoryChanged` + status.
    RememberMemory { text: String },
    /// Delete a memory by name/id; emits `Msg::MemoryChanged` + status.
    ForgetMemory { id: String },
    /// Model-assisted prune of duplicate/obsolete memories, reversible via a
    /// checkpoint. Emits `Msg::RuntimeText` (the report) + `Msg::MemoryChanged`.
    ConsolidateMemory { model_id: String },
    /// Load a specific conversation by ID and emit
    /// `Msg::ConversationLoaded`. Reducer consumes that event to
    /// replace the current session.
    LoadConversation(String),
    /// Scan the conversations directory for the /load picker. Emits
    /// `Msg::ConversationsListed` with one `ConversationSummary` per
    /// saved session (newest first). The reducer transitions to
    /// `UiMode::ConversationList` and the render shows the picker.
    ListConversations,
    /// Discover every model the user could switch to, for the `/model` picker.
    /// Emits `Msg::AvailableModelsListed`.
    ///
    /// Best-effort and strictly read-only: a dead Ollama is NOT started (a
    /// cloud-only user who stopped it to free VRAM must not have it
    /// resurrected by opening a list) and an unreachable provider is skipped
    /// rather than failing the whole listing. Provider keys are resolved
    /// inside the handler, never carried on this Cmd.
    ListAvailableModels,
    /// Walk the project for the @-mention file picker (gitignore-aware,
    /// capped, sorted). Emits `Msg::ProjectFilesListed` with relative paths
    /// (directories carry a trailing `/`).
    ListProjectFiles,
    /// List durable daemon/runtime tasks.
    ListRuntimeTasks { limit: usize },
    /// Load one durable daemon/runtime task and its timeline.
    LoadRuntimeTask { id: String },
    /// List durable daemon/runtime background processes.
    ListRuntimeProcesses { limit: usize },
    /// Print one durable process log into the conversation.
    ShowRuntimeProcessLogs { id: String },
    /// Stop a durable process by pid.
    StopRuntimeProcess { id: String },
    /// Cancel a detached background subagent (`None` = all of them). The
    /// effect layer fires the kill token held by the `SubagentSpawner`; the
    /// dying child reports back via `Msg::BackgroundAgentFinished`.
    KillBackgroundAgent { agent_id: Option<String> },
    /// Restart a durable process using its recorded command/cwd.
    RestartRuntimeProcess { id: String },
    /// Open a URL, path, or process target.
    OpenRuntimeTarget { target: String },
    /// Show listening ports.
    ShowRuntimePorts,
    /// List pending approval records.
    ListRuntimeApprovals,
    /// Mark one approval as approved or denied.
    DecideRuntimeApproval { id: String, decision: String },
    /// List restore checkpoints.
    ListRuntimeCheckpoints { limit: usize },
    /// Query checkpoints of `session_id` anchored strictly past
    /// `message_index` — fired by rewind/fork so the reducer can tell the
    /// user which file checkpoints the discarded timeline left behind.
    /// Replies with `Msg::ForkCheckpointsFound`.
    ListForkCheckpoints {
        session_id: String,
        message_index: usize,
    },
    /// List installed plugins.
    ListRuntimePlugins,
    /// Update one durable task's status.
    UpdateRuntimeTaskStatus {
        id: String,
        status: TaskStatus,
        final_report: Option<String>,
    },
    /// Create a shadow checkpoint for explicit paths.
    CreateRuntimeCheckpoint { paths: Vec<PathBuf> },
    /// Restore files from a shadow checkpoint.
    RestoreRuntimeCheckpoint { id: String },
    /// Show provider/model capability information.
    ShowRuntimeModelInfo { model: String },

    // ── MCP lifecycle ───────────────────────────────────────────────
    /// Start every configured MCP server; each one emits
    /// `Msg::McpServerReady` or `Msg::McpServerErrored` as it comes up.
    InitMcpServers(HashMap<String, McpServerConfig>),
    /// Stop a running server (e.g. config was removed, or app quit).
    StopMcpServer { name: String },

    // ── Ollama helpers ──────────────────────────────────────────────
    /// `ollama pull <model>` with progress → `Msg::ModelPullFinished`.
    PullOllamaModel { model: String },
    /// Probe whether `model_id` advertises the `vision` capability (Ollama
    /// `/api/show`) → `Msg::ProviderVisionResolved`. `warn` rides through so the
    /// reducer nags only when an image is actually in play (a paste, or a
    /// `/model` switch with an image already staged); the probe always refreshes
    /// the capability snapshot regardless.
    ProbeVision { model_id: String, warn: bool },

    // ── UI side-effects (cross-process) ─────────────────────────────
    /// `xdg-open` / `open` / `start` on a file path. Used by the
    /// image-paste preview and the "open in editor" affordance.
    OpenInSystem(PathBuf),

    // ── Attachments ─────────────────────────────────────────────────
    /// Persist a pasted image to a temp file so the TUI can open it
    /// via `OpenInSystem`. Emits no follow-up Msg on success; failure
    /// is a log-and-drop.
    WriteImageToTemp {
        path: PathBuf,
        bytes: Vec<u8>,
        format: String,
    },

    /// Read the system clipboard on a blocking task. The per-platform
    /// dispatch (xclip / wl-paste / pngpaste / PowerShell) can block
    /// for hundreds of ms on macOS via osascript, so it never runs on
    /// the reducer thread. Always emits `Msg::ClipboardRead(..)` —
    /// `Image`/`Text` on success, `Empty`/`Error` otherwise — so the
    /// paste-race guard sees every read resolve.
    ReadClipboard,

    /// Write text to the system clipboard on a blocking task (mirrors
    /// `ReadClipboard`'s per-platform dispatch). Used by in-app drag-select
    /// copy. Emits a `Msg::TransientStatus` ("Copied N chars" / failure).
    CopyToClipboard(String),

    // ── Terminal lifecycle ──────────────────────────────────────────
    /// Suspend the TUI and open `$VISUAL`/`$EDITOR` on the input draft
    /// (Ctrl+O / `/editor`). Intercepted by the interactive run loop — it
    /// owns the terminal and event stream — and never reaches the effect
    /// runner there; headless drivers log-and-drop it. The round-trip
    /// resolves as `Msg::EditorReturned`.
    ComposeInEditor { text: String },
    /// Exit the main loop. No reply message — the loop observes
    /// `state.should_exit` after the reducer returns and breaks out.
    Exit,
    /// Write the OSC 2 terminal-title sequence. Reducer diffs
    /// against `ui.last_title_dispatched` so this only fires on
    /// actual title changes, not every frame.
    SetTerminalTitle(String),
    /// Ring the terminal bell (BEL) to draw the user's attention — emitted on
    /// run completion / a pending approval only while the terminal is
    /// unfocused. Suppressed in headless mode (same gate as the title).
    AlertUser,
}

/// Inputs a model needs to generate a turn. Built by the reducer from
/// `Session` + `Settings` + current `MERMAID.md` context. Pure data —
/// no provider-specific knowledge here (that's in
/// `providers::model::*::chat`).
/// `Default` exists so adding a field costs one line here instead of a
/// mechanical edit in every construction site across providers, the CLI and
/// the tests (adding `suppressed_builtin_tools` took 15). Use
/// `..ChatRequest::default()` for the fields a caller does not care about.
#[derive(Debug, Clone, Default)]
pub struct ChatRequest {
    pub model_id: String,
    pub messages: Vec<ChatMessage>,
    pub system_prompt: String,
    /// `MERMAID.md` content to suffix onto the system prompt. `None` if
    /// no file was loaded for this project.
    pub instructions: Option<String>,
    pub reasoning: ReasoningLevel,
    pub temperature: f32,
    pub max_tokens: usize,
    /// Tool definitions advertised to the model. Combination of the
    /// built-in tool set + any advertised MCP tools from `McpState`.
    pub tools: Vec<ToolDefinition>,
    /// Per-model Ollama `num_ctx` override (`/context <n>`/`max`), or `None` to
    /// auto-fit. The one provider-specific knob that rides on the request, the
    /// same way `reasoning` does — so a live `/context` change applies on the
    /// next turn without rebuilding the cached provider. Ignored by non-Ollama
    /// providers.
    pub ollama_num_ctx: Option<u32>,
    /// Live Ollama RAM-offload toggle (`/context offload on|off`). Rides on the
    /// request like `ollama_num_ctx` so a toggle applies on the next turn without
    /// rebuilding the cached provider (whose `config` is frozen at startup).
    /// `None` falls back to the persisted `[ollama] allow_ram_offload`. Ignored
    /// by non-Ollama providers.
    pub ollama_allow_ram_offload: Option<bool>,
    /// The model's real context window, filled by the effect layer from
    /// cache-first live discovery (`resolve_context_window`) just before
    /// dispatch. The reducer always initializes it `None`; `None` at the
    /// adapter means "unknown" (no window clamp).
    pub resolved_context_window: Option<usize>,
    /// The model's real per-response output ceiling, filled alongside
    /// `resolved_context_window`. `None` means unknown — adapters that
    /// require a concrete `max_tokens` (Anthropic) fall back to a floor.
    pub resolved_max_output: Option<usize>,
    /// `mermaid run --output-schema`: the JSON Schema the FORMATTING turn
    /// must conform to. Set only on that dedicated turn (never during the
    /// agentic loop — Gemini rejects tools+schema, Ollama's `format` would
    /// degrade tool calling). When `Some`, the request carries no tools:
    /// the reducer sends none and the effect runner skips the built-ins.
    pub output_schema: Option<serde_json::Value>,
    /// Pause automatic threshold compaction for this turn. Set from
    /// `RuntimeState::auto_compact_suppressed` after an auto-compaction
    /// failure, so a chronically failing summarizer doesn't burn a model call
    /// every turn. Rides on the request like `ollama_num_ctx` because the
    /// effect preflight sees only the request, never `RuntimeState`. Cleared
    /// by a successful compaction, a manual `/compact`, or a conversation
    /// switch.
    pub suppress_auto_compact: bool,
    /// Built-in tool names the effect layer must NOT advertise on this
    /// request. Mirrors the `output_schema` empty-tools gate: the reducer
    /// (which knows the mode) decides, the effect layer (which owns the
    /// registry) enforces. Plan mode hides the checklist WRITERS here —
    /// advertising a tool whose description says "create the full initial
    /// plan" while the gate hard-errors it invites exactly that call.
    pub suppressed_builtin_tools: Vec<&'static str>,
}

/// Provider-agnostic tool definition sent in the request. Concrete
/// adapters (`providers::model::ollama`, etc.) translate this into
/// whatever wire shape their API expects.
#[derive(Debug, Clone)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

impl ToolDefinition {
    /// Wire shape: `{type: "function", function: {name, description,
    /// parameters}}`. This is the OpenAI / Ollama Chat Completions
    /// format; Anthropic and Gemini adapters translate further from
    /// here. Single-canonical-shape keeps adapters from drifting.
    pub fn to_openai_json(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description,
                "parameters": self.input_schema,
            }
        })
    }
}

impl Cmd {
    /// Human-readable tag, for tracing + replay logs. Stable across
    /// refactors (tests assert against it).
    pub fn tag(&self) -> &'static str {
        match self {
            Cmd::CallModel { .. } => "call_model",
            Cmd::CompactConversation { .. } => "compact_conversation",
            Cmd::ExecuteTool { .. } => "execute_tool",
            Cmd::CancelScope(_) => "cancel_scope",
            Cmd::BackgroundScope(_) => "background_scope",
            Cmd::ResolveApproval { .. } => "resolve_approval",
            Cmd::ResolveQuestion { .. } => "resolve_question",
            Cmd::SyncTaskStore(_) => "sync_task_store",
            Cmd::PersistPlanConfig(_) => "persist_plan_config",
            Cmd::UserTaskEdit(_) => "user_task_edit",
            Cmd::NotifyTaskCompleted { .. } => "notify_task_completed",
            Cmd::EnsureScratchpad { .. } => "ensure_scratchpad",
            Cmd::ListScratchpad { .. } => "list_scratchpad",
            Cmd::SaveConversation(_) => "save_conversation",
            Cmd::SaveCompactionArchive { .. } => "save_compaction_archive",
            Cmd::SaveProcess(_) => "save_process",
            Cmd::PersistLastModel(_) => "persist_last_model",
            Cmd::PersistReasoningFor { .. } => "persist_reasoning_for",
            Cmd::PersistOllamaNumCtxFor { .. } => "persist_ollama_num_ctx_for",
            Cmd::PersistOllamaOffload(_) => "persist_ollama_offload",
            Cmd::PersistUiTheme(_) => "persist_ui_theme",
            Cmd::ListMemory => "list_memory",
            Cmd::RememberMemory { .. } => "remember_memory",
            Cmd::ForgetMemory { .. } => "forget_memory",
            Cmd::ConsolidateMemory { .. } => "consolidate_memory",
            Cmd::LoadConversation(_) => "load_conversation",
            Cmd::ListConversations => "list_conversations",
            Cmd::ListAvailableModels => "list_available_models",
            Cmd::ListProjectFiles => "list_project_files",
            Cmd::ListRuntimeTasks { .. } => "list_runtime_tasks",
            Cmd::LoadRuntimeTask { .. } => "load_runtime_task",
            Cmd::ListRuntimeProcesses { .. } => "list_runtime_processes",
            Cmd::ShowRuntimeProcessLogs { .. } => "show_runtime_process_logs",
            Cmd::StopRuntimeProcess { .. } => "stop_runtime_process",
            Cmd::KillBackgroundAgent { .. } => "kill_background_agent",
            Cmd::RestartRuntimeProcess { .. } => "restart_runtime_process",
            Cmd::OpenRuntimeTarget { .. } => "open_runtime_target",
            Cmd::ShowRuntimePorts => "show_runtime_ports",
            Cmd::ListRuntimeApprovals => "list_runtime_approvals",
            Cmd::DecideRuntimeApproval { .. } => "decide_runtime_approval",
            Cmd::ListRuntimeCheckpoints { .. } => "list_runtime_checkpoints",
            Cmd::ListForkCheckpoints { .. } => "list_fork_checkpoints",
            Cmd::ListRuntimePlugins => "list_runtime_plugins",
            Cmd::UpdateRuntimeTaskStatus { .. } => "update_runtime_task_status",
            Cmd::CreateRuntimeCheckpoint { .. } => "create_runtime_checkpoint",
            Cmd::RestoreRuntimeCheckpoint { .. } => "restore_runtime_checkpoint",
            Cmd::ShowRuntimeModelInfo { .. } => "show_runtime_model_info",
            Cmd::InitMcpServers(_) => "init_mcp_servers",
            Cmd::StopMcpServer { .. } => "stop_mcp_server",
            Cmd::PullOllamaModel { .. } => "pull_ollama_model",
            Cmd::ProbeVision { .. } => "probe_vision",
            Cmd::OpenInSystem(_) => "open_in_system",
            Cmd::WriteImageToTemp { .. } => "write_image_to_temp",
            Cmd::ReadClipboard => "read_clipboard",
            Cmd::CopyToClipboard(_) => "copy_to_clipboard",
            Cmd::ComposeInEditor { .. } => "compose_in_editor",
            Cmd::Exit => "exit",
            Cmd::SetTerminalTitle(_) => "set_terminal_title",
            Cmd::AlertUser => "alert_user",
        }
    }

    /// True iff this command needs to run inside a `TurnScope` so it
    /// can be cancelled by `Cmd::CancelScope`. The effect runner uses
    /// this to decide between "spawn into `JoinSet`" and "spawn detached".
    pub fn is_turn_scoped(&self) -> bool {
        matches!(
            self,
            Cmd::CallModel { .. } | Cmd::CompactConversation { .. } | Cmd::ExecuteTool { .. }
        )
    }

    /// The `TurnId` of the scope this command would spawn fresh work
    /// *into*. Only the scope-spawning variants return `Some` (the same
    /// set as `is_turn_scoped`); the scope-control variants
    /// (`CancelScope` / `BackgroundScope`) act on an existing scope
    /// rather than populating one with new work, so they return `None`.
    ///
    /// The effect runner uses this to refuse spawning fresh work for a
    /// turn it has already cancelled (tombstoned) — a stray post-cancel
    /// `CallModel`/`ExecuteTool`/`CompactConversation` would otherwise
    /// resurrect an un-cancelled scope via `scope_mut`'s `or_insert_with`
    /// (F38). `CancelScope` must keep working on a tombstoned turn, which
    /// is exactly why it is excluded here.
    pub fn scope_turn(&self) -> Option<TurnId> {
        match self {
            Cmd::CallModel { turn, .. }
            | Cmd::CompactConversation { turn, .. }
            | Cmd::ExecuteTool { turn, .. } => Some(*turn),
            _ => None,
        }
    }

    /// For traces + the `--record` file — some `Cmd` payloads are huge
    /// (think `ChatRequest::messages`). This returns a compact
    /// identifier that doesn't dump the full payload.
    #[expect(
        clippy::too_many_lines,
        reason = "predates the lint; see .github/baselines/expect_budget.txt"
    )]
    pub fn summary(&self) -> String {
        match self {
            Cmd::CallModel { turn, request } => format!(
                "call_model(turn={}, model={}, msgs={})",
                turn,
                request.model_id,
                request.messages.len()
            ),
            Cmd::CompactConversation { turn, request } => format!(
                "compact_conversation(turn={}, model={}, trigger={}, msgs={})",
                turn,
                request.chat.model_id,
                request.trigger.as_str(),
                request.chat.messages.len()
            ),
            Cmd::ExecuteTool {
                turn,
                call_id,
                source,
                ..
            } => format!(
                "execute_tool(turn={}, call={}, fn={})",
                turn, call_id, source.function.name
            ),
            Cmd::CancelScope(turn) => format!("cancel_scope(turn={})", turn),
            Cmd::BackgroundScope(turn) => format!("background_scope(turn={})", turn),
            Cmd::ResolveApproval { call_id, decision } => {
                format!("resolve_approval(call={}, {:?})", call_id, decision)
            },
            Cmd::ResolveQuestion {
                call_id,
                resolution,
            } => {
                let kind = match resolution {
                    QuestionResolution::Answered { answers, .. } => {
                        format!("answered({})", answers.len())
                    },
                    QuestionResolution::Dismissed => "dismissed".to_string(),
                    QuestionResolution::Reformulate => "reformulate".to_string(),
                };
                format!("resolve_question(call={}, {})", call_id, kind)
            },
            Cmd::PersistPlanConfig(_) => "persist_plan_config".to_string(),
            Cmd::SyncTaskStore(store) => {
                format!("sync_task_store(tasks={})", store.tasks.len())
            },
            Cmd::UserTaskEdit(edit) => format!("user_task_edit({:?})", edit),
            Cmd::NotifyTaskCompleted {
                task,
                completed,
                total,
            } => format!(
                "notify_task_completed(id={}, {}/{})",
                task.id, completed, total
            ),
            Cmd::EnsureScratchpad { session_id } => {
                format!("ensure_scratchpad(session={})", session_id)
            },
            Cmd::ListScratchpad { path } => {
                format!("list_scratchpad({})", path.display())
            },
            Cmd::SaveConversation(c) => format!("save_conversation(id={})", c.id),
            Cmd::SaveCompactionArchive {
                archive, record, ..
            } => format!(
                "save_compaction_archive(conversation={}, id={})",
                archive.conversation_id, record.id
            ),
            Cmd::SaveProcess(p) => format!("save_process(id={}, pid={})", p.id, p.pid),
            Cmd::PersistLastModel(m) => format!("persist_last_model({})", m),
            Cmd::PersistReasoningFor { model_id, level } => {
                format!("persist_reasoning_for({}, {:?})", model_id, level)
            },
            Cmd::PersistOllamaNumCtxFor { model_id, num_ctx } => {
                format!("persist_ollama_num_ctx_for({}, {:?})", model_id, num_ctx)
            },
            Cmd::PersistOllamaOffload(enabled) => {
                format!("persist_ollama_offload({})", enabled)
            },
            Cmd::PersistUiTheme(theme) => format!("persist_ui_theme({})", theme.as_str()),
            Cmd::ListMemory => "list_memory".to_string(),
            Cmd::RememberMemory { .. } => "remember_memory".to_string(),
            Cmd::ForgetMemory { .. } => "forget_memory".to_string(),
            Cmd::ConsolidateMemory { .. } => "consolidate_memory".to_string(),
            Cmd::LoadConversation(id) => format!("load_conversation({})", id),
            Cmd::ListConversations => "list_conversations".to_string(),
            Cmd::ListAvailableModels => "list_available_models".to_string(),
            Cmd::ListProjectFiles => "list_project_files".to_string(),
            Cmd::ListRuntimeTasks { limit } => format!("list_runtime_tasks(limit={})", limit),
            Cmd::LoadRuntimeTask { id } => format!("load_runtime_task({})", id),
            Cmd::ListRuntimeProcesses { limit } => {
                format!("list_runtime_processes(limit={})", limit)
            },
            Cmd::ShowRuntimeProcessLogs { id } => format!("show_runtime_process_logs({})", id),
            Cmd::StopRuntimeProcess { id } => format!("stop_runtime_process({})", id),
            Cmd::KillBackgroundAgent { agent_id } => format!(
                "kill_background_agent({})",
                agent_id.as_deref().unwrap_or("all")
            ),
            Cmd::RestartRuntimeProcess { id } => format!("restart_runtime_process({})", id),
            Cmd::OpenRuntimeTarget { target } => format!("open_runtime_target({})", target),
            Cmd::ShowRuntimePorts => "show_runtime_ports".to_string(),
            Cmd::ListRuntimeApprovals => "list_runtime_approvals".to_string(),
            Cmd::DecideRuntimeApproval { id, decision } => {
                format!("decide_runtime_approval({}, {})", id, decision)
            },
            Cmd::ListForkCheckpoints {
                session_id,
                message_index,
            } => {
                format!("list_fork_checkpoints({session_id} > {message_index})")
            },
            Cmd::ListRuntimeCheckpoints { limit } => {
                format!("list_runtime_checkpoints(limit={})", limit)
            },
            Cmd::ListRuntimePlugins => "list_runtime_plugins".to_string(),
            Cmd::UpdateRuntimeTaskStatus { id, status, .. } => {
                format!("update_runtime_task_status({}, {})", id, status)
            },
            Cmd::CreateRuntimeCheckpoint { paths } => {
                format!("create_runtime_checkpoint(n={})", paths.len())
            },
            Cmd::RestoreRuntimeCheckpoint { id } => format!("restore_runtime_checkpoint({})", id),
            Cmd::ShowRuntimeModelInfo { model } => format!("show_runtime_model_info({})", model),
            Cmd::InitMcpServers(m) => format!("init_mcp_servers(n={})", m.len()),
            Cmd::StopMcpServer { name } => format!("stop_mcp_server({})", name),
            Cmd::PullOllamaModel { model } => format!("pull_ollama_model({})", model),
            Cmd::ProbeVision { model_id, warn } => format!("probe_vision({model_id}, warn={warn})"),
            Cmd::OpenInSystem(p) => format!("open_in_system({})", p.display()),
            Cmd::WriteImageToTemp {
                path,
                format,
                bytes,
            } => format!(
                "write_image_to_temp(path={}, fmt={}, n={})",
                path.display(),
                format,
                bytes.len()
            ),
            Cmd::ReadClipboard => "read_clipboard".to_string(),
            Cmd::CopyToClipboard(t) => format!("copy_to_clipboard(n={})", t.chars().count()),
            Cmd::ComposeInEditor { text } => {
                format!("compose_in_editor(n={})", text.chars().count())
            },
            Cmd::Exit => "exit".to_string(),
            Cmd::SetTerminalTitle(t) => format!("set_terminal_title({})", t),
            Cmd::AlertUser => "alert_user".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_scoped_variants_marked_correctly() {
        let request = ChatRequest {
            model_id: "m".to_string(),
            messages: vec![],
            system_prompt: String::new(),
            instructions: None,
            reasoning: ReasoningLevel::Medium,
            temperature: 0.7,
            max_tokens: 4096,
            tools: vec![],

            ollama_num_ctx: None,
            ollama_allow_ram_offload: None,
            resolved_context_window: None,
            resolved_max_output: None,
            output_schema: None,
            suppress_auto_compact: false,
            suppressed_builtin_tools: Vec::new(),
        };
        assert!(
            Cmd::CallModel {
                turn: TurnId(1),
                request,
            }
            .is_turn_scoped()
        );
        assert!(
            !Cmd::SaveConversation(ConversationHistory::new(
                "/p".to_string(),
                "m".to_string(),
                chrono::Local::now()
            ))
            .is_turn_scoped()
        );
        assert!(!Cmd::Exit.is_turn_scoped());
    }

    #[test]
    fn cmd_tags_are_stable() {
        assert_eq!(Cmd::Exit.tag(), "exit");
        assert_eq!(Cmd::CancelScope(TurnId(1)).tag(), "cancel_scope");
    }

    #[test]
    fn cmd_summary_includes_identifying_fields() {
        let c = Cmd::CancelScope(TurnId(42));
        let s = c.summary();
        assert!(s.contains("turn#42"));
    }
}
