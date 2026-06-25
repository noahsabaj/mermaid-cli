//! Shared ordered bridge for the adapter's sync `StreamCallback`.
//!
//! The four provider wrappers (ollama, anthropic, gemini, openai_compat)
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

use tokio::sync::mpsc;

use super::super::ctx::StreamEvent;

/// Construct an unbounded staging channel + spawn a relay task that
/// forwards events to `bounded_sink` in FIFO order. Returns the sender
/// callers plug into their adapter callback. When the returned sender
/// drops (last callback reference gone), the receiver closes, the relay
/// task exits cleanly.
pub fn ordered_relay(
    bounded_sink: mpsc::Sender<StreamEvent>,
) -> (
    mpsc::UnboundedSender<StreamEvent>,
    tokio::task::JoinHandle<()>,
) {
    let (tx, mut rx) = mpsc::unbounded_channel::<StreamEvent>();
    let handle = tokio::spawn(async move {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ReasoningChunk;

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
                thinking_signature: None,
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
}
