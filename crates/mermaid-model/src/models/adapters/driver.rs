//! The one streaming read loop.
//!
//! Every adapter answers the same five questions about a streaming
//! response, and only one of them is about the provider:
//!
//! 1. Is the HTTP status a success, and what error shape if not?
//! 2. How do raw TCP chunks become whole frames, and what stops an endless
//!    frame from eating memory?
//! 3. **What does one frame mean?**
//! 4. What happens to the residue left in the buffer when the body closes?
//! 5. Did the stream end, or was it cut?
//!
//! [`drive_stream`] owns 2 and 4. [`StreamProtocol::on_frame`] owns 3, and
//! [`StreamProtocol::finish`] owns 5 — each adapter's terminal-frame rule
//! differs enough (a `message_stop` event, a `finish_reason`, a `done: true`
//! field) that there is nothing to share but the question.
//!
//! Question 1 stays with the adapters. Anthropic tags a 400 that mentions
//! thinking as a signature round-trip bug, and Gemini turns
//! `PERMISSION_DENIED` into "check that `GOOGLE_API_KEY` is valid" — those are
//! the messages users actually read, and a shared handler would have to
//! grow a provider switch to keep them. The two adapters whose shape really
//! is the plain one share [`plain_http_error`] instead.
//!
//! **The protocol is sync and pure; the driver is async and owns all I/O.**
//! That split is the point:
//!
//! - Ordering is structural. `on_frame` pushes into a `Vec`, the driver
//!   drains it in order onto the bounded sink, and there is no way left to
//!   express the reordering bug that cost a spawned relay task per turn to
//!   prevent (F2).
//! - Backpressure reaches the socket, because the `await` between reads is
//!   the bounded send.
//! - Wire parsing is testable with no tokio, no HTTP and no mock server: it
//!   is a `&str -> Vec<StreamEvent>` function.

use futures::StreamExt;

use crate::models::error::{ModelError, Result};
use crate::models::stream::{StreamEvent, StreamSink, emit_all};
use crate::models::types::ModelResponse;
use crate::utils::{drain_complete_lines, drain_sse_events};

/// How a provider's byte stream splits into frames, and what that implies
/// for the un-terminated tail left when the body closes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framing {
    /// Server-Sent Events: `data:` payloads separated by a blank line. A
    /// tail with no separator is an incomplete event by definition, so it
    /// is dropped.
    Sse,
    /// Newline-delimited JSON. A server may close the body directly after
    /// the final object with no trailing newline, so the tail IS a frame
    /// and is flushed.
    Ndjson,
}

impl Framing {
    /// Drain every complete frame out of `buf`, leaving the partial tail.
    fn drain(self, buf: &mut Vec<u8>) -> Vec<String> {
        match self {
            Self::Sse => drain_sse_events(buf),
            Self::Ndjson => drain_complete_lines(buf),
        }
    }

    /// The tail, once the body has closed — `Some` only where the framing
    /// says an un-terminated tail is still a whole frame.
    fn residue(self, buf: &[u8]) -> Option<String> {
        match self {
            Self::Sse => None,
            Self::Ndjson => {
                let tail = String::from_utf8_lossy(buf);
                let trimmed = tail.trim();
                (!trimmed.is_empty()).then(|| trimmed.to_string())
            },
        }
    }

    /// What the reassembly cap is protecting against, in this framing's
    /// own words.
    const fn cap_message(self) -> &'static str {
        match self {
            Self::Sse => "SSE stream exceeded {} byte reassembly cap without a complete event",
            Self::Ndjson => "NDJSON stream exceeded {} byte reassembly cap without a complete line",
        }
    }
}

/// Whether the driver keeps reading after this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Continue,
    /// This frame was terminal. Stop reading NOW rather than waiting for
    /// the body to close — a kept-alive or proxied connection can hold it
    /// open long after the last content (#138).
    Stop,
}

/// One provider's wire format: what a frame means, and what the accumulated
/// frames add up to.
///
/// Deliberately synchronous. Everything it needs is a `&str` in and a
/// `Vec<StreamEvent>` out, so a conformance test can drive it directly with
/// recorded frames, and so the driver — not the parser — owns every
/// `await`, which is what makes the emission order the production order.
pub trait StreamProtocol {
    /// How this provider's bytes split into frames.
    const FRAMING: Framing;

    /// Consume one frame: update internal accumulators, push any events it
    /// produced onto `out`, and say whether the stream continues.
    ///
    /// Blank frames never reach here.
    ///
    /// # Errors
    ///
    /// The provider's own: a mid-stream error payload, or a frame that does
    /// not parse.
    fn on_frame(&mut self, frame: &str, out: &mut Vec<StreamEvent>) -> Result<Flow>;

    /// The stream is over. Build the response — and decide whether "over"
    /// meant finished or cut.
    ///
    /// Takes the same `out` as [`Self::on_frame`] because the terminal step
    /// legitimately produces events: OpenAI-compatible providers stream tool
    /// calls as argument fragments that are only whole once the stream ends,
    /// and an inline `<think>` tail flushes here too.
    ///
    /// # Errors
    ///
    /// The provider's own: most importantly, a body that closed before any
    /// terminal frame is a stream error and NOT a clean empty `Ok`, which
    /// would be indistinguishable from a real completion (F56).
    fn finish(self, out: &mut Vec<StreamEvent>) -> Result<ModelResponse>;
}

/// Read a response body to completion, forwarding every event the protocol
/// produces onto `sink` in order.
///
/// Takes the body rather than the `reqwest::Response` it came from: the
/// loop is about chunks of bytes, and nothing below this line is about
/// HTTP. Adapters pass `response.bytes_stream()` after their own status
/// check — see the module docs for why that one question stayed with them —
/// and tests pass a `stream::iter` of recorded chunks, which is what lets
/// the split-chunk behavior be asserted at all.
///
/// # Errors
///
/// A transport failure mid-body, a reassembly buffer that grows past
/// [`crate::constants::MAX_SSE_BUFFER_BYTES`] without ever yielding a whole
/// frame, a closed sink (the turn is gone), or whatever `on_frame` /
/// `finish` return.
pub async fn drive_stream<P, S, B, E>(
    mut body: S,
    mut protocol: P,
    sink: Option<&StreamSink>,
) -> Result<ModelResponse>
where
    P: StreamProtocol,
    S: futures::Stream<Item = std::result::Result<B, E>> + Unpin,
    B: AsRef<[u8]>,
    E: std::fmt::Display,
{
    let mut buf: Vec<u8> = Vec::new();
    let mut stopped = false;

    'read: while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|e| ModelError::StreamError(e.to_string()))?;
        // Bound reassembly: a server that streams bytes but never emits a
        // frame separator would otherwise grow `buf` without bound. At this
        // point `buf` holds only the un-terminated residue from the previous
        // drain, so this never trips on legitimately buffered whole frames
        // (#50).
        if buf.len() > crate::constants::MAX_SSE_BUFFER_BYTES {
            return Err(ModelError::StreamError(P::FRAMING.cap_message().replace(
                "{}",
                &crate::constants::MAX_SSE_BUFFER_BYTES.to_string(),
            )));
        }
        buf.extend_from_slice(chunk.as_ref());

        for frame in P::FRAMING.drain(&mut buf) {
            if frame.trim().is_empty() {
                continue;
            }
            let mut events: Vec<StreamEvent> = Vec::new();
            let flow = protocol.on_frame(&frame, &mut events)?;
            emit_all(sink, events).await?;
            if flow == Flow::Stop {
                stopped = true;
                break 'read;
            }
        }
    }

    // The tail, for framings where a body can close on a whole frame with no
    // separator after it. Skipped after `Flow::Stop`: the protocol already
    // said it had everything.
    if !stopped && let Some(frame) = P::FRAMING.residue(&buf) {
        let mut events: Vec<StreamEvent> = Vec::new();
        protocol.on_frame(&frame, &mut events)?;
        emit_all(sink, events).await?;
    }

    let mut events: Vec<StreamEvent> = Vec::new();
    let response = protocol.finish(&mut events)?;
    emit_all(sink, events).await?;
    Ok(response)
}

/// A non-success response as a bare [`BackendError::HttpError`] — status,
/// response-debug headers, body as the message.
///
/// For providers that attach no interpretable error envelope worth
/// unwrapping. Anthropic and Gemini both do, and keep their own.
pub async fn plain_http_error(response: reqwest::Response) -> ModelError {
    super::accumulator::http_error(response, "Unknown error").await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A protocol that records what it was fed. Enough to pin the driver's
    /// own responsibilities without any provider in the picture.
    struct Recorder {
        frames: Vec<String>,
        stop_after: Option<usize>,
    }

    impl Recorder {
        fn new() -> Self {
            Self {
                frames: Vec::new(),
                stop_after: None,
            }
        }
    }

    /// SSE flavour of [`Recorder`].
    struct SseRecorder(Recorder);
    /// NDJSON flavour of [`Recorder`].
    struct NdjsonRecorder(Recorder);

    impl StreamProtocol for SseRecorder {
        const FRAMING: Framing = Framing::Sse;

        fn on_frame(&mut self, frame: &str, out: &mut Vec<StreamEvent>) -> Result<Flow> {
            self.0.frames.push(frame.to_string());
            out.push(StreamEvent::Text(frame.to_string()));
            Ok(match self.0.stop_after {
                Some(n) if self.0.frames.len() >= n => Flow::Stop,
                _ => Flow::Continue,
            })
        }

        fn finish(self, _out: &mut Vec<StreamEvent>) -> Result<ModelResponse> {
            Ok(response_of(&self.0.frames))
        }
    }

    impl StreamProtocol for NdjsonRecorder {
        const FRAMING: Framing = Framing::Ndjson;

        fn on_frame(&mut self, frame: &str, out: &mut Vec<StreamEvent>) -> Result<Flow> {
            self.0.frames.push(frame.to_string());
            out.push(StreamEvent::Text(frame.to_string()));
            Ok(Flow::Continue)
        }

        fn finish(self, _out: &mut Vec<StreamEvent>) -> Result<ModelResponse> {
            Ok(response_of(&self.0.frames))
        }
    }

    fn response_of(frames: &[String]) -> ModelResponse {
        ModelResponse {
            content: frames.join("|"),
            usage: None,
            model_name: "recorder".to_string(),
            stop_reason: None,
            thinking: None,
            tool_calls: None,
            provider_continuation: None,
        }
    }

    #[test]
    fn sse_framing_drops_an_unterminated_tail() {
        // An SSE event is only whole once its blank-line separator arrives.
        let mut buf = b"data: one\n\ndata: two".to_vec();
        assert_eq!(Framing::Sse.drain(&mut buf), vec!["one".to_string()]);
        assert_eq!(Framing::Sse.residue(&buf), None);
    }

    #[test]
    fn ndjson_framing_keeps_an_unterminated_tail() {
        // The divergence this enum exists to make deliberate: an NDJSON body
        // may close directly on its final object, so the tail is a frame.
        let mut buf = b"{\"a\":1}\n{\"b\":2}".to_vec();
        assert_eq!(
            Framing::Ndjson.drain(&mut buf),
            vec!["{\"a\":1}".to_string()]
        );
        assert_eq!(Framing::Ndjson.residue(&buf), Some("{\"b\":2}".to_string()));
    }

    #[test]
    fn ndjson_residue_ignores_trailing_whitespace() {
        assert_eq!(Framing::Ndjson.residue(b"  \n  "), None);
        assert_eq!(Framing::Ndjson.residue(b""), None);
    }

    #[test]
    fn cap_message_names_the_framing_it_protects() {
        // The copy this replaced said "SSE" on the NDJSON path's sibling.
        assert!(Framing::Sse.cap_message().starts_with("SSE"));
        assert!(Framing::Ndjson.cap_message().starts_with("NDJSON"));
    }

    #[tokio::test]
    async fn frames_split_across_chunks_reassemble() {
        // The payment for the reassembly living in one place: no adapter had
        // a test for this, and now every protocol inherits one.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<StreamEvent>(16);
        let done = tokio::spawn(async move {
            drive_stream(
                chunks(&["data: hel", "lo\n\ndata: wor", "ld\n\n"]),
                SseRecorder(Recorder::new()),
                Some(&tx),
            )
            .await
        });
        let mut seen = Vec::new();
        while let Some(StreamEvent::Text(t)) = rx.recv().await {
            seen.push(t);
        }
        let out = done.await.expect("join").expect("drive");
        assert_eq!(seen, vec!["hello".to_string(), "world".to_string()]);
        assert_eq!(out.content, "hello|world");
    }

    #[tokio::test]
    async fn a_frame_delivered_one_byte_at_a_time_is_the_same_frame() {
        let body = "data: hello\n\ndata: world\n\n";
        let one_byte_each: Vec<Vec<u8>> = body.bytes().map(|b| vec![b]).collect();
        let out = drive_stream(
            byte_chunks(one_byte_each),
            SseRecorder(Recorder::new()),
            None,
        )
        .await
        .expect("drive");
        assert_eq!(out.content, "hello|world");
    }

    #[tokio::test]
    async fn stop_ends_the_read_without_waiting_for_the_body() {
        // #138: a kept-alive body can stay open long after the terminal
        // frame, so `Flow::Stop` has to leave the read loop, not just the
        // inner frame loop.
        let mut recorder = Recorder::new();
        recorder.stop_after = Some(1);
        let out = drive_stream(
            chunks(&["data: first\n\ndata: second\n\n"]),
            SseRecorder(recorder),
            None,
        )
        .await
        .expect("drive");
        assert_eq!(out.content, "first");
    }

    #[tokio::test]
    async fn ndjson_flushes_the_frame_a_body_closed_on() {
        let out = drive_stream(
            chunks(&["{\"a\":1}\n{\"b\":2}"]),
            NdjsonRecorder(Recorder::new()),
            None,
        )
        .await
        .expect("drive");
        assert_eq!(out.content, "{\"a\":1}|{\"b\":2}");
    }

    #[tokio::test]
    async fn sse_drops_the_frame_a_body_closed_on() {
        // The other half of the same decision: an SSE event with no
        // separator after it never happened.
        let out = drive_stream(
            chunks(&["data: whole\n\ndata: partial"]),
            SseRecorder(Recorder::new()),
            None,
        )
        .await
        .expect("drive");
        assert_eq!(out.content, "whole");
    }

    #[tokio::test]
    async fn a_frameless_flood_trips_the_reassembly_cap() {
        // Bytes forever, a separator never. Without the cap this is an OOM.
        let filler = "x".repeat(crate::constants::MAX_SSE_BUFFER_BYTES / 2 + 16);
        let err = drive_stream(
            chunks(&[filler.as_str(), filler.as_str(), filler.as_str()]),
            SseRecorder(Recorder::new()),
            None,
        )
        .await
        .expect_err("cap trips");
        assert!(
            matches!(&err, ModelError::StreamError(m) if m.contains("SSE stream exceeded")),
            "expected an SSE cap StreamError, got {err:?}"
        );
    }

    #[tokio::test]
    async fn a_transport_failure_mid_body_is_a_stream_error() {
        let body = futures::stream::iter(vec![
            Ok::<Vec<u8>, String>(b"data: one\n\n".to_vec()),
            Err("connection reset".to_string()),
        ]);
        let err = drive_stream(body, SseRecorder(Recorder::new()), None)
            .await
            .expect_err("transport failure");
        assert!(
            matches!(&err, ModelError::StreamError(m) if m.contains("connection reset")),
            "expected a transport StreamError, got {err:?}"
        );
    }

    /// A body that yields exactly these chunks, in order.
    fn chunks(parts: &[&str]) -> impl futures::Stream<Item = std::result::Result<Vec<u8>, String>> {
        byte_chunks(parts.iter().map(|p| p.as_bytes().to_vec()).collect())
    }

    /// [`chunks`] for a body split somewhere a `&str` cannot be — including
    /// mid-codepoint, which is exactly what the one-byte-at-a-time test is
    /// for.
    fn byte_chunks(
        parts: Vec<Vec<u8>>,
    ) -> impl futures::Stream<Item = std::result::Result<Vec<u8>, String>> {
        futures::stream::iter(parts.into_iter().map(Ok))
    }
}
