//! Runtime metadata shared by the reducer, recorder, and renderer.
//!
//! These types deliberately carry facts rather than presentation
//! strings. Tool output still contains the provider-facing text that
//! goes back into the model, while this module holds the metadata the
//! UI and future commands can consume without scraping that text.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// External lifecycle signal observed by the app shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeSignal {
    Interrupt,
    Terminate,
    Hangup,
}

impl RuntimeSignal {
    pub fn as_str(self) -> &'static str {
        match self {
            RuntimeSignal::Interrupt => "interrupt",
            RuntimeSignal::Terminate => "terminate",
            RuntimeSignal::Hangup => "hangup",
        }
    }
}

/// Runtime event recorded in state for observability / replay tooling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeTimelineEvent {
    pub kind: RuntimeTimelineKind,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeTimelineKind {
    Signal,
    Process,
    Tool,
    Provider,
}

/// Normalized provider capability snapshot exposed in app state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCapabilitySnapshot {
    pub provider: String,
    pub model: String,
    pub supports_tools: bool,
    pub supports_vision: bool,
    pub reasoning: String,
    pub max_context_tokens: Option<usize>,
    /// Per-response output ceiling, when known (static table at model-switch
    /// time; refreshed live via `ProviderContextResolved`).
    #[serde(default)]
    pub max_output_tokens: Option<usize>,
}

impl ProviderCapabilitySnapshot {
    /// Conservative static snapshot used before a provider has been
    /// resolved. This is intentionally cheap and side-effect free so
    /// the reducer can update it on `/model` without touching network
    /// or credential state.
    pub fn from_model_id(model_id: &str) -> Self {
        let (provider, model) = match model_id.split_once('/') {
            Some((provider, model)) if !provider.is_empty() && !model.is_empty() => {
                (provider.to_ascii_lowercase(), model.to_string())
            },
            _ => ("ollama".to_string(), model_id.to_string()),
        };

        let (supports_tools, supports_vision, reasoning) = match provider.as_str() {
            "anthropic" => (true, true, "adaptive".to_string()),
            "gemini" => (true, true, "thinking_level".to_string()),
            "ollama" => (true, false, "binary".to_string()),
            _ => (true, false, "effort".to_string()),
        };

        let max_context_tokens = infer_static_context_window(&provider, &model);
        // Anthropic's output ceilings are documented per family; other
        // providers start unknown and are refreshed live from `/models`
        // metadata via `ProviderContextResolved`.
        let max_output_tokens = (provider == "anthropic")
            .then(|| crate::models::adapters::anthropic::anthropic_max_output_tokens(&model));

        Self {
            provider,
            model,
            supports_tools,
            supports_vision,
            reasoning,
            max_context_tokens,
            max_output_tokens,
        }
    }
}

fn infer_static_context_window(provider: &str, model: &str) -> Option<usize> {
    // Per-model documented windows from the capability catalog (claude 200k,
    // gpt-5/gpt-4.1 400k — the latter now through ANY provider, not just
    // "openai": a deliberate accuracy widening), then per-provider fallbacks
    // for models the catalog doesn't know.
    let fallback = match provider {
        "anthropic" => Some(200_000),
        "gemini" => Some(1_000_000),
        _ => None,
    };
    crate::models::catalog::lookup(model)
        .context_window
        .or(fallback)
}

pub fn infer_static_context_window_for_model_id(model_id: &str) -> Option<usize> {
    let (provider, model) = match model_id.split_once('/') {
        Some((provider, model)) if !provider.is_empty() && !model.is_empty() => {
            (provider.to_ascii_lowercase(), model.to_string())
        },
        _ => ("ollama".to_string(), model_id.to_string()),
    };
    infer_static_context_window(&provider, &model)
}

/// Background process status tracked by Mermaid after launching a
/// command in `execute_command(mode="background")`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedProcessStatus {
    Running,
    Exited,
    Unknown,
}

/// Registry record for a background process Mermaid started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedProcess {
    pub id: String,
    pub pid: u32,
    pub command: String,
    pub cwd: Option<String>,
    pub log_path: String,
    pub detected_url: Option<String>,
    pub status: ManagedProcessStatus,
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
    #[serde(default)]
    pub artifacts: Vec<ToolArtifact>,
    /// Provider token usage the tool itself consumed (today: a subagent's
    /// cumulative child-session usage). `handle_tool_finished` folds it into
    /// the parent session's totals so the footer and the end-of-run summary
    /// count the whole tree, not just the parent's own model calls.
    #[serde(default)]
    pub token_usage: Option<crate::models::TokenUsage>,
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
    },
    WebFetch {
        url: String,
        title: Option<String>,
        line_count: usize,
        byte_count: usize,
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
    Custom {
        name: String,
        data: Value,
    },
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
    pub fn offloaded(&self) -> bool {
        self.size_vram_bytes < self.total_bytes
    }

    /// Rough percentage of the model running on CPU/RAM (0–100). Integer math;
    /// `0` when the footprint is unknown or fully resident.
    pub fn percent_on_cpu(&self) -> u8 {
        if self.total_bytes == 0 {
            return 0;
        }
        let on_cpu = self.total_bytes.saturating_sub(self.size_vram_bytes);
        (on_cpu.saturating_mul(100) / self.total_bytes) as u8
    }
}

/// Runtime state that is not part of the chat transcript sent to a
/// model, but is useful for UI, slash commands, and debugging.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeState {
    pub provider_capabilities: ProviderCapabilitySnapshot,
    #[serde(default)]
    pub processes: Vec<ManagedProcess>,
    #[serde(default)]
    pub timeline: Vec<RuntimeTimelineEvent>,
    /// Estimated token cost of the built-in tool schemas the effect runner
    /// appends to every model request during dispatch. The reducer's
    /// `/context` preview builds an MCP-only request and can't see these, so
    /// the runner reports the figure via `Msg::BuiltinToolSchemaTokens` and
    /// `/context` folds it in to match what dispatch actually decides.
    #[serde(default)]
    pub builtin_tool_schema_tokens: usize,
    /// Resolved Ollama context window for the active model (`None` until the
    /// first turn probes it, or for non-Ollama providers).
    #[serde(default)]
    pub ollama_context: Option<OllamaContextInfo>,
    /// Post-turn `/api/ps` memory placement for the active model (`None` until a
    /// turn probes it). Volatile, so it's tracked separately from the window.
    #[serde(default)]
    pub ollama_placement: Option<OllamaPlacement>,
    /// Models we've already shown the proactive auto-fit hint for this session.
    /// Session-only (not persisted) so the gentle reminder reappears each launch.
    #[serde(skip)]
    pub hinted_models: HashSet<String>,
    /// Models we've already warned about VRAM offload this session. Session-only,
    /// so the once-per-session warning behaves like the auto-fit hint.
    #[serde(skip)]
    pub offload_warned: HashSet<String>,
    /// Models we've already shown the no-vision-model notice for this session.
    /// Session-only (not persisted), so the one-shot warning behaves like the
    /// auto-fit hint and offload warning.
    #[serde(skip)]
    pub vision_warned: HashSet<String>,
    /// Auto-converge: per-model `num_ctx` that the post-turn `/api/ps` check
    /// found fits VRAM, keyed by model id. Session-only (not persisted) because
    /// it depends on whatever else is using VRAM right now; re-derived each
    /// session. Read by `build_chat_request` below a user override.
    #[serde(skip)]
    pub ollama_converged_num_ctx: std::collections::HashMap<String, u32>,
    /// When the current user interaction began. One "turn" in Mermaid is a single
    /// model call + its tools; an agentic run spans many such turns (each tool
    /// follow-up mints a fresh `TurnId`). This anchors the spinner's elapsed timer
    /// to the *whole* run so it doesn't reset to 0 at every tool step. Set on
    /// submit, read only while generating/executing tools. Session-only.
    #[serde(skip)]
    pub run_started: Option<std::time::SystemTime>,
    /// Estimated tokens generated in *completed* phases of the current run, so the
    /// spinner's token counter accumulates across tool steps instead of resetting
    /// each model call. The live phase's estimate is added on top at render time.
    #[serde(skip)]
    pub run_committed_tokens: usize,
    /// Consecutive auto-compact-and-continue recoveries in the current run after a
    /// context-window truncation. Bounded by `settings.compaction.max_truncation_recoveries`
    /// (0 = uncapped) and reset whenever the run makes progress, so it caps only
    /// no-progress thrashing on a too-small window. Session-only.
    #[serde(skip)]
    pub truncation_recoveries: u32,
    /// Consecutive turns in the current run that produced no visible output (no
    /// assistant text and no tool calls) — even if the model spent the turn on
    /// hidden reasoning. Under `MAX_EMPTY_CONTINUATIONS` the run auto-retries the
    /// model call so a stalled turn isn't left silent; at the cap it stops with a
    /// hint. Reset on a fresh run and whenever a turn makes progress. Session-only.
    #[serde(skip)]
    pub empty_continuations: u32,
    /// Consecutive auto-continuations in the current run after a response hit
    /// the provider's per-response OUTPUT cap with window room to spare
    /// (compaction can't help those — the reply is continued in a fresh turn
    /// instead). Bounded by `MAX_OUTPUT_CONTINUATIONS`; reset whenever a turn
    /// ends any other way. Session-only.
    #[serde(skip)]
    pub continue_recoveries: u32,
}

impl RuntimeState {
    pub fn new(model_id: &str) -> Self {
        Self {
            provider_capabilities: ProviderCapabilitySnapshot::from_model_id(model_id),
            processes: Vec::new(),
            timeline: Vec::new(),
            builtin_tool_schema_tokens: 0,
            ollama_context: None,
            ollama_placement: None,
            hinted_models: HashSet::new(),
            offload_warned: HashSet::new(),
            vision_warned: HashSet::new(),
            ollama_converged_num_ctx: std::collections::HashMap::new(),
            run_started: None,
            run_committed_tokens: 0,
            truncation_recoveries: 0,
            empty_continuations: 0,
            continue_recoveries: 0,
        }
    }

    /// Cap on `timeline` length. It's a recent-activity log for `/runtime` and
    /// the serialized snapshot, not an audit trail, so the oldest events are
    /// trimmed — otherwise it grows monotonically for the session's life (and
    /// it's `#[serde(default)]`, so it would also bloat every saved snapshot).
    const MAX_TIMELINE_EVENTS: usize = 200;

    /// Append a timeline event, trimming the oldest so the log stays bounded.
    fn push_timeline(&mut self, kind: RuntimeTimelineKind, message: String) {
        self.timeline.push(RuntimeTimelineEvent { kind, message });
        let len = self.timeline.len();
        if len > Self::MAX_TIMELINE_EVENTS {
            self.timeline.drain(0..len - Self::MAX_TIMELINE_EVENTS);
        }
    }

    pub fn set_model(&mut self, model_id: &str) {
        self.provider_capabilities = ProviderCapabilitySnapshot::from_model_id(model_id);
        // New model → the resolved window + placement no longer apply; re-probed
        // next turn.
        self.ollama_context = None;
        self.ollama_placement = None;
        self.push_timeline(
            RuntimeTimelineKind::Provider,
            format!("model set to {}", model_id),
        );
    }

    pub fn record_signal(&mut self, signal: RuntimeSignal) {
        self.push_timeline(
            RuntimeTimelineKind::Signal,
            format!("received {}", signal.as_str()),
        );
    }

    pub fn register_process(&mut self, process: ManagedProcess) {
        if let Some(existing) = self.processes.iter_mut().find(|p| p.pid == process.pid) {
            *existing = process.clone();
        } else {
            self.processes.push(process.clone());
        }
        self.push_timeline(
            RuntimeTimelineKind::Process,
            format!("registered process {} ({})", process.pid, process.command),
        );
    }
}

impl Default for RuntimeState {
    fn default() -> Self {
        Self::new("ollama/unknown")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_context_windows_pin_the_known_matrix() {
        // Per-model catalog rows + per-provider fallbacks, exactly as before
        // the catalog migration (plus the documented widening: gpt-4.1/gpt-5
        // get 400k through any provider now).
        for (id, want) in [
            ("anthropic/claude-sonnet-4-6", Some(200_000)),
            ("gemini/gemini-2.5-pro", Some(1_000_000)),
            ("openai/gpt-4.1", Some(400_000)),
            ("openai/gpt-5-mini", Some(400_000)),
            ("openrouter/anthropic/claude-sonnet-4.5", Some(200_000)),
            ("openai/gpt-4o", None),
            ("ollama/qwen3-coder:30b", None),
            ("anthropic/claude-future-99", Some(200_000)), // claude- catch-all row
            ("anthropic/nova-experimental", Some(200_000)), // provider fallback
        ] {
            assert_eq!(
                infer_static_context_window_for_model_id(id),
                want,
                "window for {id}"
            );
        }
    }

    #[test]
    fn anthropic_snapshot_reports_documented_output_ceiling() {
        let snap = ProviderCapabilitySnapshot::from_model_id("anthropic/claude-fable-5");
        assert_eq!(snap.max_output_tokens, Some(64_000));
        let snap = ProviderCapabilitySnapshot::from_model_id("openai/gpt-4o");
        assert_eq!(snap.max_output_tokens, None);
    }

    #[test]
    fn timeline_is_bounded_and_keeps_most_recent() {
        let mut rt = RuntimeState::new("ollama/test");
        // `new` may seed an initial event; push well past the cap and confirm
        // the log is trimmed to the most recent window rather than growing.
        for _ in 0..(RuntimeState::MAX_TIMELINE_EVENTS + 50) {
            rt.record_signal(RuntimeSignal::Interrupt);
        }
        assert_eq!(rt.timeline.len(), RuntimeState::MAX_TIMELINE_EVENTS);
        // The newest event is retained (front-trim keeps the tail).
        assert_eq!(
            rt.timeline.last().map(|e| e.message.as_str()),
            Some("received interrupt")
        );
    }
}
