//! Typed events for the model-to-UI streaming channel.
//!
//! Replaces the ad-hoc string protocol ([DONE]:tokens=N, [TOOL_CALLS:...], etc.)
//! with a typed enum that eliminates string parsing and collision risks.

use crate::models::{ToolCall, UserFacingError};

/// Events sent from the model task to the UI event loop
pub enum StreamEvent {
    /// A chunk of streamed text content from the model
    Chunk(String),
    /// Tool calls extracted from the completed model response
    ToolCalls(Vec<ToolCall>),
    /// Model finished generating (total = prompt + completion tokens)
    Done { total_tokens: usize },
    /// Model returned an error
    Error(UserFacingError),
}
