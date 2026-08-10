//! Shared ordered bridge for the adapter's sync `StreamCallback`.
//!
//! The four provider wrappers (ollama, anthropic, gemini, `openai_compat`)
//! all face the same problem: the legacy `StreamCallback` is a sync
//! `Fn(ModelStreamEvent)` closure owned by the adapter, but the v0.7
//! `StreamContext` sink is an async bounded `mpsc::Sender<StreamEvent>`.
//!
//! The obvious bridge — spawn a tokio task per callback invocation —
//! has a subtle-but-fatal ordering bug: tokio gives no guarantee that
//! tasks spawned in a sequence run in that order. A streaming response
//! of `Text("hello"), ToolCall(...), Done` could deliver `Done` first,
//! at which point the reducer commits the assistant message, transitions
//! to `Idle`, and the tool call arrives as a stale event that fails the
//! turn-id filter. The user sees the model "forget" to call the tool.
//!
//! Fix: the callback pushes into an `UnboundedSender` (synchronous send,
//! FIFO order preserved). A single relay task drains the unbounded
//! receiver and forwards each event to the real bounded sink. Bounded
//! backpressure still applies because the relay `await`s on the real
//! send — the unbounded channel just buffers briefly in between.

use std::sync::Arc;

use tokio::sync::mpsc;

use mermaid_model::models::{StreamCallback, StreamEvent};

/// Construct an unbounded staging channel + spawn a relay task that
/// forwards events to `bounded_sink` in FIFO order. Returns the sender
/// callers plug into their adapter callback. When the returned sender
/// drops (last callback reference gone), the receiver closes, the relay
/// task exits cleanly.
pub fn ordered_relay(
    bounded_sink: mpsc::Sender<StreamEvent>,
) -> (
    mpsc::UnboundedSender<StreamEvent>,
    mermaid_model::utils::AbortOnDrop,
) {
    let (tx, mut rx) = mpsc::unbounded_channel::<StreamEvent>();
    // Wrap the relay in an AbortOnDrop guard: if the parent `chat` future is
    // dropped (turn cancelled) before it `take()`s the handle to drain, the
    // relay is aborted rather than leaked — otherwise, parked on a full bounded
    // sink, it could outlive the turn.
    let handle = mermaid_model::utils::spawn_guarded(async move {
        while let Some(event) = rx.recv().await {
            if bounded_sink.send(event).await.is_err() {
                // Downstream closed — the reducer cancelled or the
                // runner is shutting down. Drop the rest silently;
                // the turn is over anyway.
                break;
            }
        }
    });
    // The handle lets the caller route the terminal `Done` through this
    // relay and then await full drain, so `Done` can never overtake a
    // still-buffered `ToolCall`/`Text` event (which would make the agent
    // "forget" to run a tool the model requested).
    (tx, handle)
}

/// Plug an adapter's sync `StreamCallback` into the staging sender from
/// [`ordered_relay`].
///
/// There is no translation left to do — the adapters and the effect layer
/// speak the same `StreamEvent` — so this is the synchronous send and
/// nothing else. Ignore errors: a closed receiver means the turn is
/// already gone.
pub fn forward_callback(sink: mpsc::UnboundedSender<StreamEvent>) -> StreamCallback {
    Arc::new(move |event: StreamEvent| {
        let _ = sink.send(event);
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mermaid_model::models::{FinishReason, ProviderContinuation, ReasoningChunk, TokenUsage};

    /// Regression guard for F2: events pushed in order must arrive in
    /// order on the bounded sink. Spawning one task per callback
    /// invocation (the pre-fix pattern) would fail this sometimes.
    #[tokio::test]
    async fn events_arrive_in_order() {
        let (sink_tx, mut sink_rx) = mpsc::channel::<StreamEvent>(16);
        let (relay, _handle) = ordered_relay(sink_tx);

        // Emit several events from a sync context (simulating the
        // adapter callback). Use mixed variants so variant identity
        // differences surface in the assertion if ordering breaks.
        relay.send(StreamEvent::Text("a".to_string())).unwrap();
        relay
            .send(StreamEvent::Reasoning(ReasoningChunk {
                text: "r1".to_string(),
                signature: None,
            }))
            .unwrap();
        relay.send(StreamEvent::Text("b".to_string())).unwrap();
        relay
            .send(StreamEvent::Done {
                usage: None,
                provider_continuation: None,
                stop_reason: None,
            })
            .unwrap();

        // Drop the sender so the relay exits after the queue drains.
        drop(relay);

        let mut seen: Vec<&'static str> = Vec::new();
        while let Some(ev) = sink_rx.recv().await {
            seen.push(match ev {
                StreamEvent::Text(s) if s == "a" => "text-a",
                StreamEvent::Text(s) if s == "b" => "text-b",
                StreamEvent::Reasoning(_) => "reasoning",
                StreamEvent::Done { .. } => "done",
                _ => "other",
            });
        }
        assert_eq!(seen, vec!["text-a", "reasoning", "text-b", "done"]);
    }
    #[tokio::test]
    async fn stream_callback_forwards_text_event() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let cb = forward_callback(tx);
        cb(StreamEvent::Text("hello".to_string()));
        let recv = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
            .await
            .expect("recv")
            .expect("sender alive");
        match recv {
            StreamEvent::Text(s) => assert_eq!(s, "hello"),
            _ => panic!("wrong variant"),
        }
    }

    #[tokio::test]
    async fn stream_callback_forwards_status_notice() {
        // The autostart notice rides the same ordered relay as content
        // events, so it reaches the effect layer (→ Msg::TransientStatus →
        // system line / stderr) rather than being swallowed as non-content.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let cb = forward_callback(tx);
        cb(StreamEvent::Status(
            "Starting the local Ollama server…".to_string(),
        ));
        let recv = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
            .await
            .expect("recv")
            .expect("sender alive");
        match recv {
            StreamEvent::Status(s) => assert!(s.contains("Starting")),
            _ => panic!("wrong variant"),
        }
    }

    #[tokio::test]
    async fn terminal_done_keeps_its_continuation_through_the_relay() {
        // The reason there is one `StreamEvent` and not two. Anthropic's
        // extended thinking only continues across turns if the opaque
        // signature reaches the reducer, and the adapter-side `Done` this
        // used to map from carried a bare `tokens: usize` — nothing to
        // rebuild a continuation out of. Same enum end to end means the
        // payload simply survives.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let cb = forward_callback(tx);
        cb(StreamEvent::Done {
            usage: Some(TokenUsage::provider(7, 11)),
            provider_continuation: Some(ProviderContinuation::Anthropic {
                signature: "opaque".to_string(),
            }),
            stop_reason: Some(FinishReason::ToolUse),
        });
        let recv = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
            .await
            .expect("recv")
            .expect("sender");
        match recv {
            StreamEvent::Done {
                usage,
                provider_continuation,
                stop_reason,
            } => {
                assert_eq!(usage.expect("usage").total_tokens(), 18);
                assert!(matches!(
                    provider_continuation,
                    Some(ProviderContinuation::Anthropic { signature }) if signature == "opaque"
                ));
                assert_eq!(stop_reason, Some(FinishReason::ToolUse));
            },
            _ => panic!("wrong variant"),
        }
    }
}
