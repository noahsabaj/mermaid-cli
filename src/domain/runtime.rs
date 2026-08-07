//! Runtime metadata shared by the reducer, recorder, and renderer.
//!
//! These types deliberately carry facts rather than presentation
//! strings. Tool output still contains the provider-facing text that
//! goes back into the model, while this module holds the metadata the
//! UI and future commands can consume without scraping that text.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

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
            "meta" => (true, true, "responses_effort".to_string()),
            "ollama" => (true, false, "binary".to_string()),
            _ => (true, false, "effort".to_string()),
        };

        // Meta's muse-spark rides the catalog like the gpt rows (its /v1/models
        // exposes no limits) — no provider special-case here.
        let max_context_tokens = infer_static_context_window(&model);
        // Output ceilings start unknown everywhere and are refreshed live via
        // `ProviderContextResolved` (for meta, from the provider's documented
        // capabilities after the first resolve — same one-turn delay as
        // anthropic/gemini).
        let max_output_tokens = None;

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

fn infer_static_context_window(model: &str) -> Option<usize> {
    // Per-model documented windows from the capability catalog — ONLY for
    // providers whose API exposes no limits (OpenAI's gpt rows). Providers
    // with a models endpoint (Anthropic, Gemini, Ollama, most OpenAI-compat)
    // resolve live via `resolve_context_window`; `None` here means "unknown
    // until discovery", never a guessed fallback.
    crate::models::catalog::lookup(model).context_window
}

pub fn infer_static_context_window_for_model_id(model_id: &str) -> Option<usize> {
    let model = match model_id.split_once('/') {
        Some((provider, model)) if !provider.is_empty() && !model.is_empty() => model,
        _ => model_id,
    };
    infer_static_context_window(model)
}

/// Tool-run value types moved to `mermaid_core::tool_run` — re-exported here so
/// `domain::ToolRunMetadata` and its siblings keep resolving unchanged.
pub use mermaid_core::tool_run::{
    ManagedProcess, ManagedProcessStatus, OllamaContextInfo, OllamaPlacement, ToolArtifact,
    ToolMetadata, ToolRunMetadata, ToolStatus, WebSearchFailure,
};

/// A subagent detached from its turn via Ctrl+B: still running in a
/// spawned task, no longer blocking the parent. Rows render in the live
/// agent panel until `Msg::BackgroundAgentFinished` removes them (and the
/// child's report arrives as a queued message).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundAgent {
    pub agent_id: String,
    pub description: String,
    pub started: std::time::SystemTime,
    #[serde(default)]
    pub activity: String,
    #[serde(default)]
    pub tokens: usize,
}

/// Runtime state that is not part of the chat transcript sent to a
/// model, but is useful for UI, slash commands, and debugging.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeState {
    pub provider_capabilities: ProviderCapabilitySnapshot,
    #[serde(default)]
    pub processes: Vec<ManagedProcess>,
    /// Subagents detached from their turn via Ctrl+B, newest last.
    #[serde(default)]
    pub background_agents: Vec<BackgroundAgent>,
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
    /// Model-call cycles since the task checklist last changed, while a task
    /// sits in_progress. Drives the staleness nudge (see `push_call_model`);
    /// session-only, reset by every `Msg::TasksUpdated`.
    #[serde(skip)]
    pub calls_since_task_update: u32,
    /// Plan-mode doom-loop breaker, armed by the FIRST plan-policy denial of
    /// the current stretch (a read-heavy Ground phase alone must never trip
    /// it). While armed, `push_plan_reminder` counts model calls; at the
    /// threshold the tail reminder escalates to a corrective. Disarmed by a
    /// successful plan write (`write_file`/`apply_patch` — the only Edit that
    /// can succeed under the plan floor) and cleared on plan entry/exit.
    /// Session-only.
    #[serde(skip)]
    pub plan_thrash_armed: bool,
    /// Model calls since the arming denial (see `plan_thrash_armed`).
    #[serde(skip)]
    pub plan_calls_since_denial: u32,
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
    /// Output tokens committed in *completed* phases of the current run
    /// (parent turns, subagents, mid-run compactions), so the spinner's token
    /// counter accumulates across tool steps instead of resetting each model
    /// call. The live phase's char-based estimate is added on top at render
    /// time.
    #[serde(skip)]
    pub run_tokens: RunTokenCounter,
    /// Lines added/removed by file-mutating tools (write_file, apply_patch)
    /// across the whole run, summed from each outcome's exact metadata counts
    /// so the end-of-run summary can show `+N/-M` without the user totting up
    /// per-call diffs. Reset on submit alongside `run_tokens`. Session-only.
    #[serde(skip)]
    pub run_line_changes: RunLineChanges,
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
    /// Auto-threshold compaction failed, so it is paused until a compaction
    /// succeeds, the user runs `/compact`, or the conversation is switched.
    /// Without this, a summarizer that keeps failing (e.g. a model that can't
    /// produce the required checkpoint structure) silently retries — and pays
    /// for — a draft + review model call on every subsequent turn. Session-only.
    #[serde(skip)]
    pub auto_compact_suppressed: bool,
}

/// Output tokens generated by the current run, with provenance. Real
/// provider counts (completion + reasoning) are the norm; a phase whose
/// provider reported no usage falls back to a chars/4 estimate and taints
/// the whole counter, so the run summary can mark the number `~`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunTokenCounter {
    pub output_tokens: usize,
    pub contains_estimate: bool,
}

impl RunTokenCounter {
    pub fn add_provider(&mut self, tokens: usize) {
        self.output_tokens = self.output_tokens.saturating_add(tokens);
    }

    pub fn add_estimate(&mut self, tokens: usize) {
        self.output_tokens = self.output_tokens.saturating_add(tokens);
        self.contains_estimate = true;
    }
}

/// Lines added/removed by file mutations in the current run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunLineChanges {
    pub added: usize,
    pub removed: usize,
}

impl RunLineChanges {
    pub fn add(&mut self, added: usize, removed: usize) {
        self.added = self.added.saturating_add(added);
        self.removed = self.removed.saturating_add(removed);
    }

    pub fn is_empty(&self) -> bool {
        self.added == 0 && self.removed == 0
    }
}

impl RuntimeState {
    pub fn new(model_id: &str) -> Self {
        Self {
            provider_capabilities: ProviderCapabilitySnapshot::from_model_id(model_id),
            processes: Vec::new(),
            background_agents: Vec::new(),
            timeline: Vec::new(),
            builtin_tool_schema_tokens: 0,
            ollama_context: None,
            ollama_placement: None,
            hinted_models: HashSet::new(),
            offload_warned: HashSet::new(),
            calls_since_task_update: 0,
            plan_thrash_armed: false,
            plan_calls_since_denial: 0,
            vision_warned: HashSet::new(),
            ollama_converged_num_ctx: std::collections::HashMap::new(),
            run_started: None,
            run_tokens: RunTokenCounter::default(),
            run_line_changes: RunLineChanges::default(),
            truncation_recoveries: 0,
            empty_continuations: 0,
            continue_recoveries: 0,
            auto_compact_suppressed: false,
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
        // The pause is model-scoped: a summarizer that couldn't produce the
        // checkpoint structure says nothing about the newly selected model,
        // and switching models is the natural user reaction to the failure.
        self.auto_compact_suppressed = false;
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
        // ONLY the OpenAI gpt rows and Meta's muse-spark keep static windows
        // (their /v1/models expose no limits). Everything else is None —
        // unknown until live discovery via `resolve_context_window` fills it.
        for (id, want) in [
            ("openai/gpt-4.1", Some(400_000)),
            ("openai/gpt-5-mini", Some(400_000)),
            ("openai/gpt-5.6", Some(1_500_000)),
            (
                "meta/muse-spark-1.1",
                Some(crate::constants::META_MUSE_SPARK_CONTEXT_WINDOW),
            ),
            (
                "meta/muse-spark-1.2",
                Some(crate::constants::META_MUSE_SPARK_CONTEXT_WINDOW),
            ),
            ("anthropic/claude-sonnet-4-6", None),
            ("gemini/gemini-2.5-pro", None),
            ("openrouter/anthropic/claude-sonnet-4.5", None),
            ("openai/gpt-4o", None),
            ("ollama/qwen3-coder:30b", None),
            ("anthropic/claude-future-99", None),
            ("anthropic/nova-experimental", None),
        ] {
            assert_eq!(
                infer_static_context_window_for_model_id(id),
                want,
                "window for {id}"
            );
        }
    }

    #[test]
    fn snapshot_limits_start_unknown_before_discovery() {
        // Pre-discovery snapshots carry no window/ceiling for providers
        // with a limits endpoint — `ProviderContextResolved` refreshes them
        // on the first turn. No static pins to rot.
        let snap = ProviderCapabilitySnapshot::from_model_id("anthropic/claude-fable-5");
        assert_eq!(snap.max_context_tokens, None);
        assert_eq!(snap.max_output_tokens, None);
        let snap = ProviderCapabilitySnapshot::from_model_id("gemini/gemini-2.5-pro");
        assert_eq!(snap.max_context_tokens, None);
        assert_eq!(snap.max_output_tokens, None);
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
