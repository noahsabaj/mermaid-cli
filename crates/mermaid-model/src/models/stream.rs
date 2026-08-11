//! Typed streaming events emitted by model adapters.
//!
//! The typed event surface is what lets adapters emit reasoning chunks,
//! tool calls, and completion signals as first-class events instead of
//! stuffing them into a text channel. Roo Code
//! (`src/api/transform/stream.rs`) and OpenCode (`provider/processor.ts`)
//! both validated this pattern as the way out of per-provider stream-shape
//! sniffing.
//!
//! This is the ONE stream event type, and `providers::ctx` re-exports it
//! rather than defining a second. A twin with a poorer `Done` needs a
//! translation layer to reach the effect layer, and a translation layer
//! cannot invent what its input never carried — which is how an opaque
//! provider continuation ends up as `None` and extended thinking stops
//! continuing across turns. `Done` here carries the whole terminal
//! payload, so there is nothing to translate.
//!
//! Events reach the turn through [`StreamSink`] — the effect layer's own
//! bounded `mpsc::Sender`, handed to the adapter as-is. There is no callback
//! and no staging channel in between: an adapter's read loop is already
//! `await`ing `stream.next()`, so it can `await` the send on the line below.

use tokio::sync::mpsc;

use super::error::{ModelError, Result};
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

/// Where a streaming chat's events go: the turn's bounded channel, owned by
/// the effect layer.
///
/// Bounded on purpose. `await`ing the send between reads is the backpressure
/// — a consumer that falls behind stalls the adapter's read loop, and the
/// provider's TCP window fills instead of a queue growing in memory.
pub type StreamSink = mpsc::Sender<StreamEvent>;

/// Best-effort out-of-band notice, for the one place that has to report
/// before a stream exists: Ollama's local-server autostart, which can block
/// ~15s behind an otherwise bare spinner.
///
/// A `&str` and not a [`StreamEvent`] because the notice has exactly one
/// shape and two destinations — the turn's sink during a chat, stderr on the
/// pre-TUI console paths — and neither wants the other's plumbing. Same shape
/// [`crate::models::adapters::ollama::LocalServerRecovery`] already uses.
pub type StatusNotify = std::sync::Arc<dyn Fn(&str) + Send + Sync>;

/// Forward one event to the turn's sink, if the caller supplied one.
///
/// # Errors
///
/// [`ModelError::StreamError`] once the receiver is gone — the turn was
/// cancelled or the runner is shutting down. Reading further bytes for a
/// response nobody will see is waste, so this is a stop and not a skip; the
/// wrapper's `select!` on the cancellation token reports the common case as
/// [`ModelError::Cancelled`] before this can fire.
pub async fn emit(sink: Option<&StreamSink>, event: StreamEvent) -> Result<()> {
    let Some(sink) = sink else {
        return Ok(());
    };
    sink.send(event)
        .await
        .map_err(|_| ModelError::StreamError("stream receiver closed".to_string()))
}

/// [`emit`] for a batch, in order.
///
/// The ordering guarantee the adapters need is this loop and nothing else:
/// events are produced into a `Vec` by synchronous wire parsing and drained
/// here. The predecessor spent an unbounded staging channel, a spawned relay
/// task and an abort guard per turn to get the same property back after a
/// `tokio::spawn` per event had taken it away (F2).
///
/// # Errors
///
/// [`emit`]'s, on the first event the closed receiver rejects.
pub async fn emit_all(
    sink: Option<&StreamSink>,
    events: impl IntoIterator<Item = StreamEvent>,
) -> Result<()> {
    for event in events {
        emit(sink, event).await?;
    }
    Ok(())
}

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
    fn sink_is_send_sync() {
        // Compile-time: the sink must satisfy Send + Sync to be carried
        // through tokio::spawn boundaries in the agent loop.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<StreamSink>();
        assert_send_sync::<StatusNotify>();
    }

    #[tokio::test]
    async fn emit_all_preserves_order_and_applies_backpressure() {
        // The whole F2 guarantee, in one loop: a batch produced by sync wire
        // parsing arrives in the order it was produced. The capacity-1 sink
        // also pins the second half of the claim — `emit_all` cannot run
        // ahead of a slow consumer, so a late `Done` can never overtake a
        // still-queued `ToolCall`.
        let (tx, mut rx) = mpsc::channel::<StreamEvent>(1);
        let batch = vec![
            StreamEvent::Text("a".to_string()),
            StreamEvent::Reasoning(ReasoningChunk {
                text: "r".to_string(),
                signature: None,
            }),
            StreamEvent::Text("b".to_string()),
            StreamEvent::Done {
                usage: None,
                provider_continuation: None,
                stop_reason: None,
            },
        ];
        let producer = tokio::spawn(async move { emit_all(Some(&tx), batch).await });

        let mut seen = Vec::new();
        while let Some(event) = rx.recv().await {
            seen.push(match event {
                StreamEvent::Text(s) => s,
                StreamEvent::Reasoning(c) => c.text,
                StreamEvent::Done { .. } => "done".to_string(),
                StreamEvent::ToolCall(_) | StreamEvent::Status(_) => "other".to_string(),
            });
        }
        producer.await.expect("join").expect("emit_all");
        assert_eq!(seen, vec!["a", "r", "b", "done"]);
    }

    #[tokio::test]
    async fn emit_stops_the_read_loop_once_the_receiver_is_gone() {
        // A dropped receiver means the turn is over. Pulling more bytes off
        // the wire for a response nobody will read is waste, so this is an
        // error the adapter propagates rather than a silently skipped send.
        let (tx, rx) = mpsc::channel::<StreamEvent>(4);
        drop(rx);
        let err = emit(Some(&tx), StreamEvent::Text("x".to_string()))
            .await
            .expect_err("closed receiver");
        assert!(matches!(err, ModelError::StreamError(_)));
    }

    #[tokio::test]
    async fn emit_without_a_sink_is_a_no_op() {
        // `chat` with no sink is the non-streaming path; helpers shared with
        // the streaming one must not have to branch on it themselves.
        emit(None, StreamEvent::Text("x".to_string()))
            .await
            .expect("no sink");
    }
}
