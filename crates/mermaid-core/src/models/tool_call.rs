//! Wire-format tool call type.
//!
//! Matches the OpenAI / Ollama `tool_calls` array shape — an `id`
//! string plus a `function` object of `{name, arguments}`. Every
//! provider adapter emits this shape via `StreamEvent::ToolCall`; the
//! reducer wraps it in a `PendingToolCall` with an internal
//! `ToolCallId` and the effect runner dispatches by `function.name`
//! into the `ToolRegistry`.

use serde::{Deserialize, Serialize};

/// A tool call from the model (OpenAI / Ollama wire format).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    #[serde(default)]
    pub id: Option<String>,
    pub function: FunctionCall,
}

/// Function call details.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: serde_json::Value,
}
