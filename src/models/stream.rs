//! Typed streaming events emitted by model adapters.
//!
//! Replaces the legacy `StreamCallback = Arc<dyn Fn(&str)>` text-only
//! callback. The typed event surface lets future adapters (Anthropic,
//! OpenAI, etc.) emit reasoning chunks, tool calls, and completion
//! signals as first-class events instead of stuffing them into the text
//! channel. Roo Code (`src/api/transform/stream.rs`) and OpenCode
//! (`provider/processor.ts`) both validated this pattern as the way out
//! of per-provider stream-shape sniffing.
//!
//! For Step 1 the only adapter is Ollama; Wave 3 wires it to emit these
//! events. Earlier waves carry the legacy text callback through a default
//! impl that synthesizes `Text` + `Done` events from the existing
//! callback shape — see `Model::chat_typed` in `traits.rs`.

use std::sync::Arc;

use super::reasoning::ReasoningChunk;
use super::tool_call::ToolCall;

/// A single event emitted during a streaming model call.
///
/// Adapters MUST emit `Done` exactly once at the end of a successful
/// stream. `Text` and `Reasoning` may interleave in any order. `ToolCall`
/// events typically arrive at the end of generation but the contract is
/// "before `Done`".
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// Plain assistant content. Append to the response buffer.
    Text(String),
    /// Reasoning / thinking content. Render separately from regular text;
    /// renderer decides whether to display or hide based on user prefs.
    Reasoning(ReasoningChunk),
    /// A tool/function call extracted from the model response.
    ToolCall(ToolCall),
    /// Stream complete. Carries the total token usage if reported by the
    /// provider; `0` if unavailable (still meaningful — the caller knows
    /// generation finished).
    Done { tokens: usize },
}

/// Callback invoked once per `StreamEvent` during a chat request.
///
/// `Send + Sync` so the callback can be shared across spawned tasks.
pub type StreamCallback = Arc<dyn Fn(StreamEvent) + Send + Sync>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_event_clone() {
        let ev = StreamEvent::Text("hello".to_string());
        let cloned = ev.clone();
        match (ev, cloned) {
            (StreamEvent::Text(a), StreamEvent::Text(b)) => assert_eq!(a, b),
            _ => panic!("clone should produce same variant"),
        }
    }

    #[test]
    fn stream_event_done_carries_tokens() {
        let ev = StreamEvent::Done { tokens: 42 };
        match ev {
            StreamEvent::Done { tokens } => assert_eq!(tokens, 42),
            _ => panic!("expected Done"),
        }
    }

    #[test]
    fn stream_event_reasoning_with_chunk() {
        let chunk = ReasoningChunk {
            text: "weighing options".to_string(),
            signature: None,
        };
        let ev = StreamEvent::Reasoning(chunk.clone());
        match ev {
            StreamEvent::Reasoning(c) => {
                assert_eq!(c.text, chunk.text);
                assert_eq!(c.signature, chunk.signature);
            },
            _ => panic!("expected Reasoning"),
        }
    }

    #[test]
    fn callback_is_send_sync() {
        // Compile-time: callback must satisfy Send + Sync to be carried
        // through tokio::spawn boundaries in the agent loop.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<StreamCallback>();
    }
}
