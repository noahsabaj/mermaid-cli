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
use crate::session::ConversationHistory;

use super::compaction::{CompactionArchive, CompactionRequest};
use super::ids::{ToolCallId, TurnId};

/// A single side-effect request. Most variants are one-shot; `CallModel`
/// and `ExecuteTool` spawn long-running tasks inside a per-turn
/// `TurnScope`.
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
    },
    /// Cancel every task in the given turn's `TurnScope`. After the
    /// scope drains, the runner emits a `Msg::StreamDone` (with a
    /// synthetic "cancelled" marker in usage, or a batch of
    /// `ToolFinished { outcome: Cancelled }` for tools already running)
    /// so the reducer can transition back to `Idle`.
    CancelScope(TurnId),

    // ── Persistence ─────────────────────────────────────────────────
    /// Save the current conversation to disk. No-op if unchanged since
    /// last save (effect-side idempotence).
    SaveConversation(ConversationHistory),
    /// Persist the raw messages removed by a compaction.
    SaveCompactionArchive(CompactionArchive),
    /// Persist the active model ID as `last_used_model`.
    PersistLastModel(String),
    /// Persist reasoning level tied to a specific model ID.
    PersistReasoningFor {
        model_id: String,
        level: ReasoningLevel,
    },
    /// Re-stat `MERMAID.md` (cheap); emits `Msg::InstructionsChanged`
    /// only when the mtime moved or the file appeared/disappeared.
    RefreshInstructions,
    /// Load a specific conversation by ID and emit
    /// `Msg::ConversationLoaded`. Reducer consumes that event to
    /// replace the current session.
    LoadConversation(String),
    /// Scan the conversations directory for the /load picker. Emits
    /// `Msg::ConversationsListed` with one `ConversationSummary` per
    /// saved session (newest first). The reducer transitions to
    /// `UiMode::ConversationList` and the render shows the picker.
    ListConversations,

    // ── MCP lifecycle ───────────────────────────────────────────────
    /// Start every configured MCP server; each one emits
    /// `Msg::McpServerReady` or `Msg::McpServerErrored` as it comes up.
    InitMcpServers(HashMap<String, McpServerConfig>),
    /// Stop a running server (e.g. config was removed, or app quit).
    StopMcpServer { name: String },

    // ── Ollama helpers ──────────────────────────────────────────────
    /// `ollama pull <model>` with progress → `Msg::ModelPullFinished`.
    PullOllamaModel { model: String },

    // ── UI side-effects (cross-process) ─────────────────────────────
    /// `xdg-open` / `open` / `start` on a file path. Used by the
    /// image-paste preview and the "open in editor" affordance.
    OpenInSystem(PathBuf),

    // ── Status line ─────────────────────────────────────────────────
    /// Schedule `Msg::StatusDismiss` after `ms` milliseconds. Reducer
    /// uses this to self-clear transient status lines.
    DismissStatusAfter { ms: u64 },

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
    /// the reducer thread. Emits `Msg::Paste(Paste::Image|Text)` on
    /// success; `Msg::TransientStatus` when the clipboard is empty or
    /// the read fails.
    ReadClipboard,

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
            Cmd::SaveConversation(_) => "save_conversation",
            Cmd::SaveCompactionArchive(_) => "save_compaction_archive",
            Cmd::PersistLastModel(_) => "persist_last_model",
            Cmd::PersistReasoningFor { .. } => "persist_reasoning_for",
            Cmd::RefreshInstructions => "refresh_instructions",
            Cmd::LoadConversation(_) => "load_conversation",
            Cmd::ListConversations => "list_conversations",
            Cmd::InitMcpServers(_) => "init_mcp_servers",
            Cmd::StopMcpServer { .. } => "stop_mcp_server",
            Cmd::PullOllamaModel { .. } => "pull_ollama_model",
            Cmd::OpenInSystem(_) => "open_in_system",
            Cmd::DismissStatusAfter { .. } => "dismiss_status_after",
            Cmd::WriteImageToTemp { .. } => "write_image_to_temp",
            Cmd::ReadClipboard => "read_clipboard",
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
                model_id: _,
            } => format!(
                "execute_tool(turn={}, call={}, fn={})",
                turn, call_id, source.function.name
            ),
            Cmd::CancelScope(turn) => format!("cancel_scope(turn={})", turn),
            Cmd::SaveConversation(c) => format!("save_conversation(id={})", c.id),
            Cmd::SaveCompactionArchive(a) => format!(
                "save_compaction_archive(conversation={}, id={})",
                a.conversation_id, a.id
            ),
            Cmd::PersistLastModel(m) => format!("persist_last_model({})", m),
            Cmd::PersistReasoningFor { model_id, level } => {
                format!("persist_reasoning_for({}, {:?})", model_id, level)
            },
            Cmd::RefreshInstructions => "refresh_instructions".to_string(),
            Cmd::LoadConversation(id) => format!("load_conversation({})", id),
            Cmd::ListConversations => "list_conversations".to_string(),
            Cmd::InitMcpServers(m) => format!("init_mcp_servers(n={})", m.len()),
            Cmd::StopMcpServer { name } => format!("stop_mcp_server({})", name),
            Cmd::PullOllamaModel { model } => format!("pull_ollama_model({})", model),
            Cmd::OpenInSystem(p) => format!("open_in_system({})", p.display()),
            Cmd::DismissStatusAfter { ms } => format!("dismiss_status_after({}ms)", ms),
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
        };
        assert!(
            Cmd::CallModel {
                turn: TurnId(1),
                request,
            }
            .is_turn_scoped()
        );
        assert!(
            !Cmd::SaveConversation(ConversationHistory::new("/p".to_string(), "m".to_string()))
                .is_turn_scoped()
        );
        assert!(!Cmd::RefreshInstructions.is_turn_scoped());
        assert!(!Cmd::Exit.is_turn_scoped());
    }

    #[test]
    fn cmd_tags_are_stable() {
        assert_eq!(Cmd::Exit.tag(), "exit");
        assert_eq!(Cmd::RefreshInstructions.tag(), "refresh_instructions");
        assert_eq!(Cmd::CancelScope(TurnId(1)).tag(), "cancel_scope");
    }

    #[test]
    fn cmd_summary_includes_identifying_fields() {
        let c = Cmd::CancelScope(TurnId(42));
        let s = c.summary();
        assert!(s.contains("turn#42"));
    }
}
