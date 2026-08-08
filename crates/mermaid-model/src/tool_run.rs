//! Value types describing one tool run — pure data, no runtime.
//!
//! These carry facts rather than presentation strings: tool output still holds
//! the provider-facing text that goes back to the model, while this module
//! holds the metadata the UI and future commands consume without scraping it.
//!
//! They live in `mermaid-model` rather than in `domain` because `ChatMessage`
//! embeds them (through `ActionDisplay`), and a wire type reaching up into the
//! MVU layer for its own field types was the last cycle standing between the
//! model layer and its own crate.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Registry record for a background process Mermaid started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedProcess {
    pub id: String,
    pub pid: u32,
    pub command: String,
    pub cwd: Option<String>,
    pub log_path: String,
    pub detected_url: Option<String>,
    pub status: mermaid_runtime::ProcessStatus,
}

/// Structured metadata extracted from a completed tool run.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolRunMetadata {
    #[serde(default)]
    pub detail: ToolMetadata,
    pub line_count: Option<usize>,
    pub byte_count: Option<usize>,
    pub result_count: Option<usize>,
    pub duration_secs: Option<f64>,
    pub process: Option<ManagedProcess>,
    /// User-facing display diff for file mutations. This is captured
    /// at tool execution time so whole-file writes can compare against
    /// the pre-write contents even after the file has been overwritten.
    #[serde(default)]
    pub display_diff: Option<String>,
    #[serde(default)]
    pub diff_truncated: bool,
    /// Exact line-change counts for file mutations. Carried separately from
    /// `display_diff` because that string is capped at
    /// `MAX_DISPLAY_DIFF_LINES` — recounting it would undercount large
    /// writes. `handle_tool_finished` folds these into the per-run totals
    /// behind the end-of-run `+N/-M` summary.
    #[serde(default)]
    pub lines_added: usize,
    #[serde(default)]
    pub lines_removed: usize,
    #[serde(default)]
    pub artifacts: Vec<ToolArtifact>,
    /// Provider token usage the tool itself consumed (today: a subagent's
    /// cumulative child-session usage). `handle_tool_finished` folds it into
    /// the parent session's totals so the footer and the end-of-run summary
    /// count the whole tree, not just the parent's own model calls.
    #[serde(default)]
    pub token_usage: Option<crate::models::TokenUsage>,
    /// This call wrote the plan file while planning — the FACT the doom-loop
    /// breaker disarms on.
    ///
    /// Recorded at the boundary that actually knows it (the policy gate
    /// approved the write, or the file mutator targeted the plan path) rather
    /// than inferred from the tool name. Inferring it missed the shell
    /// spelling entirely: the escalated corrective tells the model "a shell
    /// redirect writing ONLY that file works too", and when the model complied
    /// the breaker stayed armed and kept re-injecting "the plan file does not
    /// exist until you write it" at a model that had just written it.
    #[serde(default)]
    pub plan_file_written: bool,
}

/// Tool outcome status independent of how the result is rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Success,
    Error,
    Cancelled,
}

/// Typed metadata produced by a specific tool implementation.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolMetadata {
    #[default]
    None,
    ReadFile {
        paths: Vec<String>,
        line_count: usize,
        byte_count: usize,
        truncated: bool,
    },
    WriteFile {
        path: String,
        line_count: usize,
        byte_count: usize,
        created: Option<bool>,
    },
    ApplyPatch {
        added: Vec<String>,
        modified: Vec<String>,
        deleted: Vec<String>,
        renamed: Vec<(String, String)>,
        fuzzy: bool,
    },
    DeleteFile {
        path: String,
    },
    CreateDirectory {
        path: String,
    },
    WebSearch {
        queries: Vec<String>,
        requested_count: usize,
        result_count: usize,
        sources: Vec<String>,
        #[serde(default)]
        backend: String,
        #[serde(default)]
        succeeded_queries: usize,
        #[serde(default)]
        failed_queries: usize,
        #[serde(default)]
        partial: bool,
        #[serde(default)]
        truncated: bool,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        failures: Vec<WebSearchFailure>,
    },
    WebFetch {
        /// Sanitized originally requested URL.
        url: String,
        #[serde(default)]
        final_url: Option<String>,
        #[serde(default)]
        status: Option<u16>,
        #[serde(default)]
        error_kind: Option<String>,
        #[serde(default)]
        media_type: Option<String>,
        #[serde(default)]
        charset: Option<String>,
        #[serde(default)]
        backend: String,
        #[serde(default)]
        extraction: String,
        title: Option<String>,
        line_count: usize,
        byte_count: usize,
        #[serde(default)]
        source_byte_count: usize,
        /// Bytes in the extracted page before the 30 KiB rendered envelope is
        /// applied. This may exceed the bytes retained in a bounded snapshot;
        /// `truncated` records that distinction.
        #[serde(default)]
        output_byte_count: usize,
        #[serde(default)]
        truncated: bool,
        #[serde(default)]
        pattern: Option<String>,
        #[serde(default)]
        context_lines: Option<usize>,
        #[serde(default)]
        match_count: Option<usize>,
        #[serde(default)]
        snapshot_id: Option<String>,
    },
    ExecuteCommand {
        command: String,
        working_dir: Option<String>,
        exit_code: Option<i32>,
        timed_out: bool,
        background: bool,
        stdout_lines: usize,
        stderr_lines: usize,
        detected_urls: Vec<String>,
        pid: Option<u32>,
        log_path: Option<String>,
        /// The command was terminated by the OS sandbox (e.g. it tried to
        /// reach the network under `--no-network`). Additive; `#[serde(default)]`
        /// keeps older recordings/rows deserializable.
        #[serde(default)]
        denied_by_sandbox: bool,
    },
    ComputerUse {
        action: String,
        params: Value,
    },
    Mcp {
        server: String,
        tool: String,
    },
    Subagent {
        model_id: String,
        /// Continuation handle: pass back via the `agent` tool's `agent_id`
        /// arg to send a follow-up prompt to this child with its context
        /// intact. Empty on recordings from before continuations existed.
        #[serde(default)]
        agent_id: String,
    },
    /// The task checklist tools (`task_create` / `task_update` / `task_list`).
    /// `action` is the wire tool suffix ("create" / "update" / "list");
    /// counts are over visible (non-deleted) tasks after the call.
    Tasks {
        action: String,
        completed: u32,
        total: u32,
    },
    /// `ask_user_question` resolved with answers. Kept structured so the
    /// transcript can replay each question → answer pair rather than a bare
    /// duration.
    Questions {
        answers: Vec<crate::question::QuestionAnswer>,
        /// The answers came from remembered cross-session preferences
        /// (`memoryKey`) rather than a live prompt.
        #[serde(default)]
        remembered: bool,
    },
    /// `exit_plan_mode` resolved with an APPROVED plan: the transcript
    /// renders the plan body as a markdown block, and `handle_tool_finished`
    /// keys the post-approval mechanics (clear `session.plan`, seed the
    /// checklist, optionally auto-submit) on this variant. A
    /// request-for-changes outcome carries no metadata.
    Plan {
        /// Plan-file path as shown to the user (project-relative).
        path: String,
        /// The approved plan text, re-read from disk at approval time.
        body: String,
        /// True when the user chose to start implementing immediately.
        #[serde(default)]
        start: bool,
        /// Execution begins in a FRESH conversation seeded with the handoff
        /// preamble + plan (clear-context execute, or a fresh-session
        /// handoff). The exploration context is left behind on disk.
        #[serde(default)]
        fresh: bool,
        /// Handoff variant that copies the transcript into a new
        /// conversation before starting (mutually exclusive with `fresh`).
        #[serde(default)]
        fork: bool,
        /// Handoff: switch the session to this model for execution.
        #[serde(default)]
        model: Option<String>,
    },
    Custom {
        name: String,
        data: Value,
    },
}

/// One failed item from an ordered web-search batch. The index preserves its
/// relationship to the input without copying potentially sensitive query text
/// into telemetry; `error` is redacted before construction.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WebSearchFailure {
    /// Zero-based index into the original `queries` array.
    pub query_index: usize,
    /// Secret-redacted, byte-bounded backend failure detail.
    pub error: String,
}

/// Non-text artifact produced by a tool. Images are base64 strings to
/// match the existing chat-message storage format.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolArtifact {
    Image { data: String },
    File { path: String },
    Log { path: String },
}

/// The resolved Ollama context window for the active model, reported by the
/// effect runner after the first turn. Drives the `/context` display and the
/// truncation quick-fix. `model_max` is the probed architectural window;
/// `effective` is the `num_ctx` we actually send.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct OllamaContextInfo {
    pub model_max: Option<usize>,
    pub effective: Option<usize>,
    pub source: Option<crate::models::adapters::ollama_sizing::NumCtxSource>,
}

/// Post-turn memory placement of the loaded Ollama model, from `/api/ps`.
/// `total_bytes` is weights + KV + buffers; `size_vram_bytes` is the part
/// resident in VRAM. Volatile (changes when the model reloads), so it lives
/// outside the quasi-static [`OllamaContextInfo`].
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct OllamaPlacement {
    pub size_vram_bytes: u64,
    pub total_bytes: u64,
}

impl OllamaPlacement {
    /// True when the model didn't fully fit VRAM and spilled to CPU/RAM (slow).
    #[must_use]
    pub fn offloaded(&self) -> bool {
        self.size_vram_bytes < self.total_bytes
    }

    /// Rough percentage of the model running on CPU/RAM (0–100). Integer math;
    /// `0` when the footprint is unknown or fully resident.
    #[must_use]
    pub fn percent_on_cpu(&self) -> u8 {
        if self.total_bytes == 0 {
            return 0;
        }
        let on_cpu = self.total_bytes.saturating_sub(self.size_vram_bytes);
        (on_cpu.saturating_mul(100) / self.total_bytes) as u8
    }
}
