//! Typed streaming events emitted by model adapters.
//!
//! The typed event surface is what lets adapters emit reasoning chunks,
//! tool calls, and completion signals as first-class events instead of
//! stuffing them into a text channel. Roo Code
//! (`src/api/transform/stream.rs`) and OpenCode (`provider/processor.ts`)
//! both validated this pattern as the way out of per-provider stream-shape
//! sniffing.
//!
//! This is the ONE stream event type. `providers::ctx` re-exports it: the
//! CLI used to define a structurally-identical twin whose only difference
//! was a richer `Done`, and `stream_bridge::forward_callback` existed to
//! map one onto the other variant by variant. The `Done` here is that
//! richer one, so there is nothing left to map.

use std::sync::Arc;

use super::reasoning::ReasoningChunk;
use super::tool_call::ToolCall;
use super::types::{FinishReason, ProviderContinuation, TokenUsage};

/// A single event emitted during a streaming model call.
///
/// Exactly one `Done` ends a successful stream. `Text` and `Reasoning` may
/// interleave in any order. `ToolCall` events typically arrive at the end
/// of generation but the contract is "before `Done`".
///
/// Adapters themselves never emit `Done` — the provider wrapper builds the
/// authoritative one from the returned `ModelResponse`, which is where the
/// usage and the provider continuation actually live (F3).
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// Plain assistant content. Append to the response buffer.
    Text(String),
    /// Reasoning / thinking content. Render separately from regular text;
    /// renderer decides whether to display or hide based on user prefs.
    Reasoning(ReasoningChunk),
    /// A tool/function call extracted from the model response.
    ToolCall(ToolCall),
    /// Out-of-band, user-visible plumbing notice (e.g. "Starting the local
    /// Ollama server…"). NOT response content: surfaces as a transient /
    /// system line, never appended to the assistant message. May arrive
    /// before any `Text`.
    Status(String),
    /// Stream complete. Carries final token usage (`None` when the provider
    /// never reported any, so the reducer keeps its estimate rather than
    /// resetting the gauge to zero), any opaque provider continuation state
    /// to round-trip on the next request, and why generation stopped (so the
    /// reducer can flag truncation or a content block).
    Done {
        usage: Option<TokenUsage>,
        provider_continuation: Option<ProviderContinuation>,
        stop_reason: Option<FinishReason>,
    },
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
    fn stream_event_done_carries_the_whole_terminal_payload() {
        // The reason this type is shared rather than mapped: `Done` has to
        // reach the reducer with the continuation intact. The adapter-side
        // `Done` used to carry a bare `tokens: usize`, so anything mapped
        // from it lost all three of these, and the wrapper had to route its
        // authoritative `Done` around the mapping to compensate.
        let ev = StreamEvent::Done {
            usage: Some(TokenUsage::provider(10, 20)),
            provider_continuation: Some(ProviderContinuation::Anthropic {
                signature: "sig".to_string(),
            }),
            stop_reason: Some(FinishReason::ToolUse),
        };
        match ev {
            StreamEvent::Done {
                usage,
                provider_continuation,
                stop_reason,
            } => {
                assert_eq!(usage.expect("usage").total_tokens(), 30);
                assert!(matches!(
                    provider_continuation,
                    Some(ProviderContinuation::Anthropic { .. })
                ));
                assert_eq!(stop_reason, Some(FinishReason::ToolUse));
            },
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
