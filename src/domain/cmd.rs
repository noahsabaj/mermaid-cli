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
use crate::models::ChatMessage;
use crate::models::ReasoningLevel;
use crate::models::tool_call::ToolCall as ModelToolCall;
use crate::runtime::{SafetyMode, TaskStatus};
use crate::session::ConversationHistory;

use super::state::ApprovalChoice;

use super::compaction::{CompactionArchive, CompactionRecord, CompactionRequest};
use super::ids::{ToolCallId, TurnId};
use super::runtime::ManagedProcess;

/// A single side-effect request. Most variants are one-shot; `CallModel`
/// and `ExecuteTool` spawn long-running tasks inside a per-turn
/// `TurnScope`.
// Several variants legitimately carry large payloads (a full
// `ConversationHistory` / `ChatRequest`). Boxing them would churn ~20
// construction + match sites for no real gain — `Cmd` values are short-lived
// and moved, not stored in bulk.
#[allow(clippy::large_enum_variant)]
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
        /// Effective live safety mode (from `state.session.safety_mode`) at
        /// the moment this call was emitted. The runner builds the policy
        /// gate / Auto classifier from this rather than the static config.
        safety_mode: SafetyMode,
        /// The user's stated intent for the turn (latest user message),
        /// passed to the Auto-mode classifier as alignment context.
        intent: Option<String>,
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
        record: CompactionRecord,
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
    /// Exit the main loop. No reply message — the loop observes
    /// `state.should_exit` after the reducer returns and breaks out.
    Exit,
    /// Write the OSC 2 terminal-title sequence. Reducer diffs
    /// against `ui.last_title_dispatched` so this only fires on
    /// actual title changes, not every frame.
    SetTerminalTitle(String),
}

/// Inputs a model needs to generate a turn. Built by the reducer from
/// `Session` + `Settings` + current `MERMAID.md` context. Pure data —
/// no provider-specific knowledge here (that's in
/// `providers::model::*::chat`).
#[derive(Debug, Clone)]
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
            Cmd::SaveConversation(_) => "save_conversation",
            Cmd::SaveCompactionArchive { .. } => "save_compaction_archive",
            Cmd::SaveProcess(_) => "save_process",
            Cmd::PersistLastModel(_) => "persist_last_model",
            Cmd::PersistReasoningFor { .. } => "persist_reasoning_for",
            Cmd::PersistOllamaNumCtxFor { .. } => "persist_ollama_num_ctx_for",
            Cmd::PersistOllamaOffload(_) => "persist_ollama_offload",
            Cmd::ListMemory => "list_memory",
            Cmd::RememberMemory { .. } => "remember_memory",
            Cmd::ForgetMemory { .. } => "forget_memory",
            Cmd::ConsolidateMemory { .. } => "consolidate_memory",
            Cmd::LoadConversation(_) => "load_conversation",
            Cmd::ListConversations => "list_conversations",
            Cmd::ListRuntimeTasks { .. } => "list_runtime_tasks",
            Cmd::LoadRuntimeTask { .. } => "load_runtime_task",
            Cmd::ListRuntimeProcesses { .. } => "list_runtime_processes",
            Cmd::ShowRuntimeProcessLogs { .. } => "show_runtime_process_logs",
            Cmd::StopRuntimeProcess { .. } => "stop_runtime_process",
            Cmd::RestartRuntimeProcess { .. } => "restart_runtime_process",
            Cmd::OpenRuntimeTarget { .. } => "open_runtime_target",
            Cmd::ShowRuntimePorts => "show_runtime_ports",
            Cmd::ListRuntimeApprovals => "list_runtime_approvals",
            Cmd::DecideRuntimeApproval { .. } => "decide_runtime_approval",
            Cmd::ListRuntimeCheckpoints { .. } => "list_runtime_checkpoints",
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
            Cmd::Exit => "exit",
            Cmd::SetTerminalTitle(_) => "set_terminal_title",
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
            Cmd::ListMemory => "list_memory".to_string(),
            Cmd::RememberMemory { .. } => "remember_memory".to_string(),
            Cmd::ForgetMemory { .. } => "forget_memory".to_string(),
            Cmd::ConsolidateMemory { .. } => "consolidate_memory".to_string(),
            Cmd::LoadConversation(id) => format!("load_conversation({})", id),
            Cmd::ListConversations => "list_conversations".to_string(),
            Cmd::ListRuntimeTasks { limit } => format!("list_runtime_tasks(limit={})", limit),
            Cmd::LoadRuntimeTask { id } => format!("load_runtime_task({})", id),
            Cmd::ListRuntimeProcesses { limit } => {
                format!("list_runtime_processes(limit={})", limit)
            },
            Cmd::ShowRuntimeProcessLogs { id } => format!("show_runtime_process_logs({})", id),
            Cmd::StopRuntimeProcess { id } => format!("stop_runtime_process({})", id),
            Cmd::RestartRuntimeProcess { id } => format!("restart_runtime_process({})", id),
            Cmd::OpenRuntimeTarget { target } => format!("open_runtime_target({})", target),
            Cmd::ShowRuntimePorts => "show_runtime_ports".to_string(),
            Cmd::ListRuntimeApprovals => "list_runtime_approvals".to_string(),
            Cmd::DecideRuntimeApproval { id, decision } => {
                format!("decide_runtime_approval({}, {})", id, decision)
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
            Cmd::Exit => "exit".to_string(),
            Cmd::SetTerminalTitle(t) => format!("set_terminal_title({})", t),
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
