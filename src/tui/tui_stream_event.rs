//! Typed events for the model-to-UI streaming channel.
//!
//! TUI-specific event shape carried over the mpsc channel between the
//! model task spawned in `loop_coordinator::TuiObserver::call_model` and
//! the rendering loop. Distinct from `models::StreamEvent` (the new
//! adapter-level typed events) — this one carries `UserFacingError` for
//! in-band error transport, which the model-layer enum doesn't know
//! about. The two shapes mean different things at different layers; the
//! `Tui` prefix prevents accidental confusion at import sites.

use crate::models::{ToolCall, UserFacingError};

/// Events sent from the model task to the UI event loop.
pub enum TuiStreamEvent {
    /// A chunk of streamed text content from the model.
    Chunk(String),
    /// Tool calls extracted from the completed model response.
    ToolCalls(Vec<ToolCall>),
    /// Model finished generating (total = prompt + completion tokens).
    Done { total_tokens: usize },
    /// Model returned an error.
    Error(UserFacingError),
}
