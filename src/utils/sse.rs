//! Server-Sent Events parser for OpenAI-compatible streaming endpoints.
//!
//! OpenAI Chat Completions and every conformant clone (Groq, Together,
//! Fireworks, OpenRouter, vLLM, DeepInfra, Cerebras, …) stream responses
//! as SSE: each event is one or more `field: value` lines, and events
//! are separated by a blank line (`\n\n`). The data we care about lives
//! in the `data:` field; all other fields (`event:`, `id:`, `retry:`,
//! comments starting with `:`) are ignored. Stream end is signaled by a
//! sentinel event with the literal payload `[DONE]`.
//!
//! Mirrors the pattern in `crate::utils::ndjson::drain_complete_lines`:
//! a byte buffer accumulates raw network bytes (so chunks split inside
//! UTF-8 codepoints don't corrupt anything — the `\n\n` event separator
//! cannot appear inside a multi-byte UTF-8 sequence), and only complete
//! events are returned. Partial trailing events stay in the buffer for
//! the next call.

/// Drain all complete SSE events out of `buf`, returning the payload of
/// each `data:` field. The `[DONE]` sentinel and non-`data:` fields are
/// filtered out. Each returned `String` is exactly one event's data
/// (multi-line `data:` fields are joined with `\n`, per the SSE spec).
pub fn drain_sse_events(buf: &mut Vec<u8>) -> Vec<String> {
    let mut events = Vec::new();

    while let Some(end) = find_event_boundary(buf) {
        // Drain through the boundary inclusive, then strip the trailing
        // separator before parsing the event body.
        let raw: Vec<u8> = buf.drain(..end).collect();
        let body = strip_trailing_separator(&raw);
        let text = String::from_utf8_lossy(body);

        let mut data_lines: Vec<&str> = Vec::new();
        for line in text.split('\n') {
            // SSE spec: lines starting with `:` are comments. Lines
            // without a colon are field-only with empty value (we don't
            // care about those for `data:`).
            if line.starts_with(':') || line.is_empty() {
                continue;
            }
            if let Some(value) = line.strip_prefix("data:") {
                // Per spec, a single leading space after the colon is
                // ignored. Don't trim further — the payload may have
                // significant trailing whitespace.
                let value = value.strip_prefix(' ').unwrap_or(value);
                data_lines.push(value);
            }
            // Other fields (event:, id:, retry:) ignored for now.
        }

        if data_lines.is_empty() {
            continue;
        }
        let payload = data_lines.join("\n");
        // The terminal sentinel — the caller knows the stream is done
        // when the underlying HTTP body closes. No need to surface this.
        if payload == "[DONE]" {
            continue;
        }
        events.push(payload);
    }

    events
}

/// Find the byte offset (exclusive end) of the next complete SSE event in
/// `buf`. Per the SSE spec, events are separated by either `\n\n` or
/// `\r\n\r\n`. Returns `None` if no complete event is buffered yet.
fn find_event_boundary(buf: &[u8]) -> Option<usize> {
    // `\n\n` is the common Linux/macOS shape; `\r\n\r\n` is the spec-
    // canonical shape some servers emit. Find the earliest of the two.
    let lf_lf = find_subsequence(buf, b"\n\n");
    let crlf_crlf = find_subsequence(buf, b"\r\n\r\n");
    match (lf_lf, crlf_crlf) {
        (Some(a), Some(b)) => Some(std::cmp::min(a + 2, b + 4)),
        (Some(a), None) => Some(a + 2),
        (None, Some(b)) => Some(b + 4),
        (None, None) => None,
    }
}

/// Strip the trailing `\n\n` or `\r\n\r\n` separator from a drained event.
fn strip_trailing_separator(raw: &[u8]) -> &[u8] {
    if raw.ends_with(b"\r\n\r\n") {
        &raw[..raw.len() - 4]
    } else if raw.ends_with(b"\n\n") {
        &raw[..raw.len() - 2]
    } else {
        raw
    }
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::drain_sse_events;

    #[test]
    fn empty_buffer_yields_nothing() {
        let mut buf: Vec<u8> = Vec::new();
        assert!(drain_sse_events(&mut buf).is_empty());
        assert!(buf.is_empty());
    }

    #[test]
    fn complete_event_is_drained() {
        let mut buf = b"data: hello\n\n".to_vec();
        let events = drain_sse_events(&mut buf);
        assert_eq!(events, vec!["hello".to_string()]);
        assert!(buf.is_empty());
    }

    #[test]
    fn multiple_events_in_one_chunk() {
        let mut buf = b"data: one\n\ndata: two\n\ndata: three\n\n".to_vec();
        let events = drain_sse_events(&mut buf);
        assert_eq!(
            events,
            vec!["one".to_string(), "two".to_string(), "three".to_string()]
        );
        assert!(buf.is_empty());
    }

    #[test]
    fn done_sentinel_filtered() {
        let mut buf = b"data: real\n\ndata: [DONE]\n\n".to_vec();
        let events = drain_sse_events(&mut buf);
        assert_eq!(events, vec!["real".to_string()]);
    }

    #[test]
    fn comment_and_event_field_ignored() {
        let mut buf = b": this is a heartbeat\n\nevent: chunk\ndata: payload\n\n".to_vec();
        let events = drain_sse_events(&mut buf);
        assert_eq!(events, vec!["payload".to_string()]);
    }

    #[test]
    fn partial_event_left_in_buffer() {
        let mut buf = b"data: complete\n\ndata: partial".to_vec();
        let events = drain_sse_events(&mut buf);
        assert_eq!(events, vec!["complete".to_string()]);
        assert_eq!(buf, b"data: partial");
    }

    #[test]
    fn event_split_across_chunks_reassembles() {
        let mut buf: Vec<u8> = Vec::new();
        // Chunk 1: complete event header but no separator yet.
        buf.extend_from_slice(b"data: split me");
        assert!(drain_sse_events(&mut buf).is_empty());
        // Chunk 2: arrives with the separator.
        buf.extend_from_slice(b"\n\n");
        let events = drain_sse_events(&mut buf);
        assert_eq!(events, vec!["split me".to_string()]);
    }

    #[test]
    fn cjk_payload_split_across_chunks_preserved() {
        // A 3-byte CJK char ("你" = E4 BD A0) split across chunks must
        // survive reassembly intact — same guarantee as drain_complete_lines.
        // Splitting on `\n\n` is safe because neither byte appears inside
        // any multi-byte UTF-8 sequence.
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(b"data: \xE4\xBD"); // first 2 bytes of "你"
        assert!(drain_sse_events(&mut buf).is_empty());
        buf.extend_from_slice(b"\xA0\xE5\xA5\xBD\n\n"); // last byte of "你" + "好" + separator
        let events = drain_sse_events(&mut buf);
        assert_eq!(events, vec!["你好".to_string()]);
    }

    #[test]
    fn crlf_separator_is_handled() {
        let mut buf = b"data: spec-canonical\r\n\r\n".to_vec();
        let events = drain_sse_events(&mut buf);
        assert_eq!(events, vec!["spec-canonical".to_string()]);
    }

    #[test]
    fn done_sentinel_with_crlf_is_consumed_not_emitted() {
        // #11 regression: with CRLF framing the `[DONE]` sentinel must match
        // exactly (no trailing `\r`), so it's swallowed rather than surfaced as
        // a bogus event that would fail JSON parsing downstream.
        let mut buf = b"data: real\r\n\r\ndata: [DONE]\r\n\r\n".to_vec();
        let events = drain_sse_events(&mut buf);
        assert_eq!(events, vec!["real".to_string()]);
    }

    #[test]
    fn multiline_data_field_joined_with_newline() {
        // Per SSE spec, multiple `data:` lines in one event are joined
        // with `\n` to form the payload. Rare in practice for our
        // providers (they put JSON on a single line) but the spec is the
        // spec.
        let mut buf = b"data: line one\ndata: line two\n\n".to_vec();
        let events = drain_sse_events(&mut buf);
        assert_eq!(events, vec!["line one\nline two".to_string()]);
    }
}
