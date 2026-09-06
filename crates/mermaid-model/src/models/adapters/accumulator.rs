//! Stream-accumulation rules every adapter obeys, in one place.
//!
//! Five adapters used to carry a private copy of each rule below, and the
//! copies drifted exactly the way copies do: Meta shipped with no response
//! cap at all, Anthropic reported a provider usage of zero when no usage
//! frame ever arrived, four of five put a 4xx body into the error unredacted
//! (Meta's copy redacted, with a comment explaining a risk that applied to
//! all five), and every adapter shared one truncation flag between its
//! reasoning and its content, so a reasoning trace past the cap silently
//! emptied the answer that followed it. A rule that lives here is one the
//! sixth adapter cannot forget to copy.

use crate::constants::{MAX_RESPONSE_CHARS, MAX_TOOL_ARG_BYTES, MAX_TOOL_CALLS};
use crate::models::error::{BackendError, ModelError, ResponseDebugContext};
use crate::models::types::FinishReason;

/// Appended once when a capped buffer stops accepting text.
pub(super) const TRUNCATION_MARKER: &str = "\n\n[TRUNCATED: response exceeded size limit]";

/// The most of a provider's error body worth keeping: enough for any real
/// diagnostic, small enough that a hostile endpoint cannot buffer gigabytes
/// into a `ModelError`.
pub(super) const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;

/// A text accumulator with a hard byte cap. Past the cap it appends
/// [`TRUNCATION_MARKER`] once and ignores everything after. The flag is per
/// buffer: a reasoning trace that trips its cap leaves the content buffer
/// accepting text, which is the whole reason this is a type and not a
/// `(String, &mut bool)` pair.
#[derive(Debug)]
pub(super) struct CappedText {
    buf: String,
    cap: usize,
    truncated: bool,
}

impl Default for CappedText {
    fn default() -> Self {
        Self::new()
    }
}

impl CappedText {
    /// A buffer capped at `MAX_RESPONSE_CHARS`. `const`, unlike `Default`, so
    /// a `const fn` constructor can hold one.
    pub(super) const fn new() -> Self {
        Self::with_cap(MAX_RESPONSE_CHARS)
    }

    pub(super) const fn with_cap(cap: usize) -> Self {
        Self {
            buf: String::new(),
            cap,
            truncated: false,
        }
    }

    /// Append `chunk`, cutting at a char boundary if it crosses the cap. A
    /// full buffer ignores the chunk entirely.
    pub(super) fn push(&mut self, chunk: &str) {
        push_capped(&mut self.buf, chunk, &mut self.truncated, self.cap);
    }

    /// `true` until the cap trips. Adapters gate the stream event on this so
    /// the UI stops receiving text the accumulator is dropping.
    pub(super) fn accepting(&self) -> bool {
        !self.truncated
    }

    #[cfg(test)]
    pub(super) fn truncated(&self) -> bool {
        self.truncated
    }

    #[cfg(test)]
    pub(super) fn as_str(&self) -> &str {
        &self.buf
    }

    pub(super) fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub(super) fn into_string(self) -> String {
        self.buf
    }
}

/// The primitive behind [`CappedText`], for accumulators whose buffers live
/// inside an enum and cannot own a `CappedText` (Anthropic's per-block
/// buffers). Appends `chunk` char-boundary-safe up to `cap`, marks once, and
/// is a no-op after `*truncated` is set. Keep the flag PER BUFFER: one flag
/// shared between reasoning and content is how a long trace used to empty
/// the answer.
pub(super) fn push_capped(buf: &mut String, chunk: &str, truncated: &mut bool, cap: usize) {
    if *truncated {
        return;
    }
    buf.push_str(chunk);
    if buf.len() > cap {
        let end = buf.floor_char_boundary(cap);
        buf.truncate(end);
        buf.push_str(TRUNCATION_MARKER);
        *truncated = true;
    }
}

/// Append a streaming tool-argument fragment, hard-capping the buffer at
/// `MAX_TOOL_ARG_BYTES`. A crafted stream could otherwise send unbounded
/// fragments and grow this buffer without limit (the daemon is long-lived).
/// Past the cap we stop appending at a char boundary; the now-truncated JSON
/// simply fails to parse and falls back to a raw string -- bounded, not an
/// OOM (#14).
pub(super) fn push_tool_arg(buf: &mut String, frag: &str) {
    let cap = MAX_TOOL_ARG_BYTES;
    if buf.len() >= cap {
        return;
    }
    let room = cap - buf.len();
    if frag.len() <= room {
        buf.push_str(frag);
    } else {
        let end = frag.floor_char_boundary(room);
        buf.push_str(&frag[..end]);
    }
}

/// Whether a stream-supplied index may open a new tool-call or content-block
/// slot. Every adapter keys some map or vector on an index the provider
/// chooses; without this bound a well-framed hostile stream grows it without
/// limit, one small frame at a time.
pub(super) fn slot_in_bounds(index: usize) -> bool {
    index < MAX_TOOL_CALLS
}

/// Parse a reassembled tool-argument buffer. A parse failure falls back to the
/// raw text (the tool then reports invalid arguments, as before) but the
/// reason is logged rather than dropped: the one diagnostic that says the
/// argument cap fired, or that the provider emitted malformed JSON, used to
/// vanish at exactly the site where it was in hand.
pub(super) fn parse_tool_args(tool: &str, raw: String) -> serde_json::Value {
    match serde_json::from_str::<serde_json::Value>(&raw) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(
                tool,
                bytes = raw.len(),
                %error,
                "tool arguments did not parse; passing the raw text through"
            );
            serde_json::Value::String(raw)
        },
    }
}

/// A stream that ended without its terminal frame closed abnormally: the
/// connection dropped, a proxy cut it, or the server crashed mid-response
/// (F56). Every provider marks the end with a finish reason, so "none seen"
/// is the signal. `Length` is a finish reason too -- a real truncation is a
/// completed stream, not a dropped one.
pub(super) fn ended_without_terminal(finish_reason: Option<&FinishReason>) -> bool {
    finish_reason.is_none()
}

/// Read a failed response's body: at most [`MAX_ERROR_BODY_BYTES`], then
/// through `redact_secrets`. A 4xx from an OpenAI-compatible gateway, relay
/// or self-hosted proxy routinely echoes the request -- `Authorization:
/// Bearer sk-...` included -- and this text is rendered into the transcript
/// and persisted with it.
pub(super) async fn error_body(mut response: reqwest::Response, fallback: &str) -> String {
    let mut buf: Vec<u8> = Vec::new();
    let mut truncated = false;
    // The end of the body, or a transport error mid-body, ends the read
    // either way: what arrived is what there is.
    while let Ok(Some(bytes)) = response.chunk().await {
        let room = MAX_ERROR_BODY_BYTES.saturating_sub(buf.len());
        if bytes.len() > room {
            buf.extend_from_slice(&bytes[..room]);
            truncated = true;
            break;
        }
        buf.extend_from_slice(&bytes);
    }
    if buf.is_empty() {
        return fallback.to_string();
    }
    let mut text = String::from_utf8_lossy(&buf).into_owned();
    if truncated {
        text.push_str("\n[error body truncated]");
    }
    crate::utils::redact_secrets(&text)
}

/// The generic HTTP failure: status, capped and redacted body, debug headers.
/// Providers that attach an error envelope worth unwrapping (Anthropic,
/// Gemini) read the body through [`error_body`] and parse it themselves.
pub(super) async fn http_error(response: reqwest::Response, fallback: &str) -> ModelError {
    let status = response.status().as_u16();
    let debug = ResponseDebugContext::from_headers(response.headers());
    let message = error_body(response, fallback).await;
    ModelError::Backend(BackendError::HttpError {
        status,
        message,
        debug,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capped_text_marks_once_and_then_ignores() {
        let mut t = CappedText::with_cap(10);
        t.push("hello");
        assert!(t.accepting());
        t.push(" world!!");
        assert!(t.truncated());
        assert!(!t.accepting());
        let cut = t.as_str().to_string();
        t.push("more");
        assert_eq!(t.as_str(), cut, "a full buffer ignores later chunks");
        assert!(cut.starts_with("hello worl"));
        assert!(cut.ends_with(TRUNCATION_MARKER));
        assert_eq!(cut.matches(TRUNCATION_MARKER).count(), 1);
    }

    #[test]
    fn capped_text_cuts_at_a_char_boundary() {
        let mut t = CappedText::with_cap(4);
        t.push("你你你你");
        assert!(t.truncated());
        assert!(t.as_str().starts_with('你'));
        assert!(t.as_str().is_char_boundary(3));
    }

    #[test]
    fn reasoning_and_content_truncate_independently() {
        let mut reasoning = CappedText::with_cap(8);
        let mut content = CappedText::with_cap(8);
        reasoning.push("a very long thought");
        assert!(reasoning.truncated());
        assert!(
            content.accepting(),
            "one buffer's cap must not gate the other"
        );
        content.push("answer");
        assert_eq!(content.as_str(), "answer");
    }

    #[test]
    fn tool_arg_buffer_is_bounded() {
        let mut buf = String::new();
        push_tool_arg(&mut buf, &"x".repeat(MAX_TOOL_ARG_BYTES + 100));
        assert_eq!(buf.len(), MAX_TOOL_ARG_BYTES);
        push_tool_arg(&mut buf, "more");
        assert_eq!(buf.len(), MAX_TOOL_ARG_BYTES);
    }

    #[test]
    fn slot_bound_matches_the_tool_call_cap() {
        assert!(slot_in_bounds(0));
        assert!(slot_in_bounds(MAX_TOOL_CALLS - 1));
        assert!(!slot_in_bounds(MAX_TOOL_CALLS));
    }

    #[test]
    fn unparseable_tool_args_fall_back_to_the_raw_text() {
        assert_eq!(
            parse_tool_args("t", r#"{"a":1}"#.to_string()),
            serde_json::json!({"a": 1})
        );
        assert_eq!(
            parse_tool_args("t", r#"{"a":"#.to_string()),
            serde_json::Value::String(r#"{"a":"#.to_string())
        );
    }

    #[test]
    fn a_missing_finish_reason_is_an_abnormal_close() {
        assert!(ended_without_terminal(None));
        assert!(!ended_without_terminal(Some(&FinishReason::Length)));
        assert!(!ended_without_terminal(Some(&FinishReason::Stop)));
    }

    fn response_with_body(status: u16, body: String) -> reqwest::Response {
        http::Response::builder()
            .status(status)
            .body(body)
            .expect("test response")
            .into()
    }

    #[tokio::test]
    async fn error_body_is_redacted() {
        let body = r#"{"error":"bad header: Authorization: Bearer sk-abcdefghijklmnopqrstuvwxyz0123456789"}"#;
        let text = error_body(response_with_body(401, body.to_string()), "fallback").await;
        assert!(!text.contains("sk-abcdefghijklmnopqrstuvwxyz"), "{text}");
        assert!(text.contains("bad header"), "{text}");
    }

    #[tokio::test]
    async fn error_body_is_capped() {
        let body = "x".repeat(MAX_ERROR_BODY_BYTES * 3);
        let text = error_body(response_with_body(500, body), "fallback").await;
        assert!(text.len() < MAX_ERROR_BODY_BYTES + 64, "{}", text.len());
        assert!(text.ends_with("[error body truncated]"));
    }

    #[tokio::test]
    async fn an_empty_error_body_reads_as_the_fallback() {
        let text = error_body(
            response_with_body(502, String::new()),
            "gateway said nothing",
        )
        .await;
        assert_eq!(text, "gateway said nothing");
        let err = http_error(response_with_body(503, String::new()), "down").await;
        match err {
            ModelError::Backend(BackendError::HttpError {
                status, message, ..
            }) => {
                assert_eq!(status, 503);
                assert_eq!(message, "down");
            },
            other => panic!("expected HttpError, got {other:?}"),
        }
    }
}
