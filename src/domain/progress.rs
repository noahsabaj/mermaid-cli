//! Tool-progress vocabulary.
//!
//! `ProgressEvent` is part of the reducer's INPUT alphabet: it rides in
//! `Msg::ToolProgress` and is dispatched by `handle_tool_progress`. It lived in
//! `providers::ctx`, which put an edge from the pure MVU core into the impure
//! shell for a plain serde value type. The invariant worth being able to state
//! is that every type reachable from `Msg` is defined at or below the domain.
//!
//! Not pushed further down into `mermaid-model`: a tool-progress event is not a
//! model-layer concept and does not belong next to the wire adapters.

use mermaid_model::ids::ToolCallId;

/// Tool-side progress event. The reducer already knows `ToolStarted`
/// and `ToolFinished`; this carries everything in between (streaming
/// subprocess output, long-running download status, multimodal
/// artifacts like inline screenshots, and nested activity from
/// subagents).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum ProgressEvent {
    /// Partial stdout/stderr chunk.
    Output(String),
    /// Arbitrary status string for display.
    Status(String),
    /// Byte-count progress for long downloads/transfers. `total` is
    /// None when the producer doesn't know the final size.
    Bytes { done: u64, total: Option<u64> },
    /// Binary artifact produced mid-execution (screenshot preview,
    /// generated file, etc.). MIME string determines routing in the
    /// reducer — `image/*` attaches inline to the active assistant
    /// message; anything else lands on the status line as a label.
    Artifact {
        mime: String,
        #[serde(with = "mermaid_model::utils::serde_base64")]
        data: Vec<u8>,
        caption: Option<String>,
    },
    /// A child subagent just started or finished a tool call. Carries
    /// the CHILD's call identity + tool name + phase so the parent UI
    /// can surface it without needing to recurse into the child's
    /// event vocabulary.
    SubagentToolCall {
        child_call_id: ToolCallId,
        tool_name: String,
        phase: SubagentPhase,
    },
    /// Coarse phase label for a child subagent ("starting…",
    /// "thinking", "replying"). Emitted only on phase CHANGE — never
    /// per stream chunk — so the parent status stays calm.
    SubagentActivity(String),
    /// Cumulative output-token estimate for a child subagent's current
    /// drive. Throttled at the source (≥500ms apart); powers the live
    /// per-agent token counters without per-chunk churn.
    SubagentTokens(usize),
}

/// Phase a subagent tool-call is in, from the parent's perspective.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SubagentPhase {
    Started,
    Finished,
    Errored,
}
