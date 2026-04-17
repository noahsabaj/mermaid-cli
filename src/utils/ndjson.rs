//! UTF-8-safe NDJSON line draining for byte-stream readers.
//!
//! Newline-delimited JSON over an HTTP byte stream presents two pitfalls:
//! TCP chunk boundaries don't align with line boundaries, *and* they don't
//! align with UTF-8 codepoint boundaries (a 3-byte CJK char can straddle
//! two packets). Decoding each chunk independently with `from_utf8_lossy`
//! would corrupt those split codepoints into U+FFFD.
//!
//! `drain_complete_lines` solves this by buffering raw bytes and only
//! decoding *complete* lines: splitting on the single byte `b'\n'`
//! (0x0A) is safe because that byte never appears inside a multi-byte
//! UTF-8 sequence, and by the time `\n` arrives, every byte of every
//! preceding codepoint has also arrived.

/// Drain all complete newline-terminated lines out of `buf`, decoding each
/// as UTF-8 and dropping the trailing `\n`. Any partial trailing line stays
/// in `buf` so the caller can append more bytes and try again.
pub fn drain_complete_lines(buf: &mut Vec<u8>) -> Vec<String> {
    let mut lines = Vec::new();
    while let Some(newline_pos) = buf.iter().position(|&b| b == b'\n') {
        // Drain through the `\n` inclusive, then drop it before decoding.
        let line_bytes: Vec<u8> = buf.drain(..=newline_pos).collect();
        let content = &line_bytes[..line_bytes.len() - 1];
        lines.push(String::from_utf8_lossy(content).into_owned());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::drain_complete_lines;

    #[test]
    fn drain_empty_buf() {
        let mut buf: Vec<u8> = Vec::new();
        assert!(drain_complete_lines(&mut buf).is_empty());
        assert!(buf.is_empty());
    }

    #[test]
    fn drain_without_newline_keeps_buf_intact() {
        let mut buf = b"partial".to_vec();
        assert!(drain_complete_lines(&mut buf).is_empty());
        assert_eq!(buf, b"partial");
    }

    #[test]
    fn drain_yields_complete_lines_and_keeps_tail() {
        let mut buf = b"first\nsecond\ntail".to_vec();
        let lines = drain_complete_lines(&mut buf);
        assert_eq!(lines, vec!["first".to_string(), "second".to_string()]);
        assert_eq!(buf, b"tail");
    }

    /// A 3-byte CJK char ("你" = E4 BD A0) split across chunks must survive
    /// reassembly intact — no U+FFFD. Simulates the TCP-boundary-splits-a-
    /// codepoint scenario the helper is guarding against.
    #[test]
    fn drain_preserves_codepoints_across_chunks() {
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(b"hello ");
        buf.extend_from_slice(&[0xE4, 0xBD]);
        assert!(drain_complete_lines(&mut buf).is_empty());

        buf.extend_from_slice(&[0xA0]);
        buf.extend_from_slice("好\n".as_bytes());

        let lines = drain_complete_lines(&mut buf);
        assert_eq!(lines, vec!["hello 你好".to_string()]);
        assert!(buf.is_empty());
    }
}
