//! Value types describing one tool action's display.
//!
//! These ride alongside `ChatMessage::actions` — the chat renderer
//! paints one per tool call attached to an assistant message. Pure
//! data; no runtime, no I/O.
//!
//! Lives in `mermaid-core` because `models::ChatMessage` embeds a
//! `Vec<ActionDisplay>`; the wire type owning display data is a wart, but it is
//! serialized into every saved conversation, so the type moves down rather than
//! the field going away.
//!
//! New tool executions land here via `domain::transition::
//! action_display_for`, which reads the `PendingToolCall` and
//! `ToolOutcome` produced by the reducer. The chat widget then
//! paints the `ActionDetails` variant's specific layout.

use serde::{Deserialize, Serialize};

use crate::tool_run::ToolRunMetadata;

/// Result shape carried on an `ActionDisplay`. Discriminates the
/// success/error path; the chat widget colors them differently and
/// `Success` may carry image data for multimodal tools.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[must_use]
pub enum ActionResult {
    Success {
        output: String,
        #[serde(default)]
        images: Option<Vec<String>>,
    },
    Error {
        error: String,
    },
    /// Call is still in flight. Only ever synthesized by the render layer
    /// for the live transcript view (the chat widget blinks the header dot);
    /// never committed to the message log, so it never reaches disk.
    Running,
}

/// Attached to a committed assistant `ChatMessage` — one per tool
/// call that ran during that turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionDisplay {
    /// Human-readable kind ("Read", "Bash", "Update", "Web Search", …).
    pub action_type: String,
    /// Target string (file path, command, query, …).
    pub target: String,
    /// Success / error outcome.
    pub result: ActionResult,
    /// Type-specific display data.
    #[serde(default)]
    pub details: ActionDetails,
    /// Wall-clock time in seconds, for long-running operations.
    pub duration_seconds: Option<f64>,
    /// Structured facts about the tool run. Optional for old saved
    /// conversations and simple actions.
    #[serde(default)]
    pub metadata: Option<ToolRunMetadata>,
}

/// Per-action-type display payload. The chat widget matches on this
/// to render code blocks, diffs, agent summaries, etc.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub enum ActionDetails {
    /// No extra data — delete, mkdir, old history without richer
    /// info.
    #[default]
    Simple,
    /// Plain preview with optional line count (read, command output,
    /// web search results).
    Preview {
        text: String,
        line_count: Option<usize>,
    },
    /// Whole-file write — renders syntax-highlighted preview.
    FileContent { line_count: usize, content: String },
    /// Targeted edit — renders summary + diff.
    Diff { summary: String, diff: String },
}
