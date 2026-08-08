//! Bound-before-allocate read helpers (Cause 2).
//!
//! `std::fs::read`, `AsyncBufReadExt::read_line`, and friends size their
//! allocation to whatever the source hands them. When the source is untrusted
//! — an MCP server's stdout, a daemon client's command line, a file whose size
//! a model chose — that's an unbounded-allocation (OOM) vector. These helpers
//! cap the read *before* the bytes land in memory.

use std::io::Read;
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

/// Outcome of one [`read_line_capped`] call.
pub enum CappedLine {
    /// A complete line, with the trailing newline (and any `\r`) stripped,
    /// within the byte cap.
    Line(Vec<u8>),
    /// The line exceeded the cap. The stream has been advanced past the
    /// offending line (drained to the next newline) so reading can resume on
    /// the following frame — one oversize line doesn't kill the connection.
    TooLong,
    /// Clean end of stream: no more data and no partial line pending.
    Eof,
}

/// Read one newline-delimited line from `reader`, refusing to buffer more than
/// `max_bytes` before the newline.
///
/// Unlike [`AsyncBufReadExt::read_line`]/`lines()`, a peer that streams bytes
/// without ever sending `\n` cannot drive unbounded allocation here: once the
/// pending line passes `max_bytes` the buffered bytes are dropped and we only
/// scan forward for the next newline to resync.
///
/// # Errors
///
/// Only the underlying reader's I/O errors. Exceeding the cap is not one of
/// them: that is `Ok(CappedLine::TooLong)`, and so is a clean EOF
/// (`CappedLine::Eof`), because both are states a caller resumes from rather
/// than a broken stream.
pub async fn read_line_capped<R>(reader: &mut R, max_bytes: usize) -> std::io::Result<CappedLine>
where
    R: AsyncBufRead + Unpin,
{
    let mut buf: Vec<u8> = Vec::new();
    let mut overflow = false;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            // EOF.
            if overflow {
                return Ok(CappedLine::TooLong);
            }
            if buf.is_empty() {
                return Ok(CappedLine::Eof);
            }
            // Final line with no trailing newline.
            return Ok(CappedLine::Line(strip_trailing_cr(buf)));
        }
        if let Some(nl) = available.iter().position(|&b| b == b'\n') {
            if !overflow {
                if buf.len() + nl > max_bytes {
                    overflow = true;
                } else {
                    buf.extend_from_slice(&available[..nl]);
                }
            }
            reader.consume(nl + 1);
            if overflow {
                return Ok(CappedLine::TooLong);
            }
            return Ok(CappedLine::Line(strip_trailing_cr(buf)));
        }
        // No newline in this fill — consume it and keep scanning.
        let n = available.len();
        if !overflow {
            if buf.len() + n > max_bytes {
                // Past the cap: drop what we have and just resync to the next
                // newline (we only need the buffer's *length* bounded).
                overflow = true;
                buf = Vec::new();
            } else {
                buf.extend_from_slice(available);
            }
        }
        reader.consume(n);
    }
}

fn strip_trailing_cr(mut v: Vec<u8>) -> Vec<u8> {
    if v.last() == Some(&b'\r') {
        v.pop();
    }
    v
}

/// Read at most `max_bytes` from `path`. Returns `(bytes, truncated)` where
/// `bytes.len() <= max_bytes` and `truncated` is `true` when the file was
/// longer than the cap.
///
/// Unlike [`std::fs::read`], a multi-gigabyte file never pulls more than
/// `max_bytes` (+1 probe byte) into memory.
///
/// # Errors
///
/// Opening `path` (missing, not readable, a directory), then any read error.
/// A file longer than the cap is not an error — that is the `truncated` flag.
pub fn read_file_capped(
    path: &std::path::Path,
    max_bytes: usize,
) -> std::io::Result<(Vec<u8>, bool)> {
    read_capped(std::fs::File::open(path)?, max_bytes)
}

/// Like [`read_file_capped`], but reads from an already-open [`std::fs::File`].
///
/// Used when the caller has obtained the handle through a confinement-checked
/// open (e.g. [`mermaid_runtime::open_beneath`]) and must read the *same* inode
/// the check resolved — feeding a path back to [`read_file_capped`] would reopen
/// by name and reintroduce the symlink TOCTOU (#77).
///
/// # Errors
///
/// Only read errors on the handle the caller already opened. A file longer
/// than the cap is not an error — that is the `truncated` flag.
pub fn read_capped(file: std::fs::File, max_bytes: usize) -> std::io::Result<(Vec<u8>, bool)> {
    let mut buf = Vec::new();
    // Read one byte past the cap so we can distinguish "exactly at cap" from
    // "over cap" without a separate stat (which would race the read anyway).
    let cap_plus = (max_bytes as u64).saturating_add(1);
    file.take(cap_plus).read_to_end(&mut buf)?;
    let truncated = buf.len() > max_bytes;
    if truncated {
        buf.truncate(max_bytes);
    }
    Ok((buf, truncated))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn read_all(input: &[u8], cap: usize) -> Vec<CappedLine> {
        let mut reader = tokio::io::BufReader::new(input);
        let mut out = Vec::new();
        loop {
            match read_line_capped(&mut reader, cap).await.unwrap() {
                CappedLine::Eof => break,
                other => out.push(other),
            }
        }
        out
    }

    fn lines(outcomes: &[CappedLine]) -> Vec<String> {
        outcomes
            .iter()
            .map(|o| match o {
                CappedLine::Line(b) => String::from_utf8_lossy(b).into_owned(),
                CappedLine::TooLong => "<TOOLONG>".to_string(),
                CappedLine::Eof => "<EOF>".to_string(),
            })
            .collect()
    }

    #[tokio::test]
    async fn splits_lines_and_strips_crlf() {
        let out = read_all(b"alpha\r\nbeta\ngamma", 1024).await;
        assert_eq!(lines(&out), vec!["alpha", "beta", "gamma"]);
    }

    #[tokio::test]
    async fn empty_input_is_eof() {
        let out = read_all(b"", 1024).await;
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn oversize_line_is_rejected_then_stream_resyncs() {
        // A 5000-byte line with a 16-byte cap → TooLong, but the *next* line
        // (within cap) is still delivered: one bad frame can't wedge the reader.
        let big = "x".repeat(5000);
        let input = format!("{big}\nok\n");
        let out = read_all(input.as_bytes(), 16).await;
        assert_eq!(lines(&out), vec!["<TOOLONG>", "ok"]);
    }

    #[tokio::test]
    async fn line_exactly_at_cap_is_kept() {
        // Exactly `cap` content bytes before the newline must be accepted.
        let input = b"0123456789\n"; // 10 content bytes
        let out = read_all(input, 10).await;
        assert_eq!(lines(&out), vec!["0123456789"]);
    }

    #[test]
    fn read_file_capped_truncates_large_files() {
        let dir = std::env::temp_dir().join(format!("bounded_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("big.txt");
        std::fs::write(&path, vec![b'a'; 1000]).unwrap();

        let (bytes, truncated) = read_file_capped(&path, 100).unwrap();
        assert_eq!(bytes.len(), 100);
        assert!(truncated);

        let (bytes, truncated) = read_file_capped(&path, 5000).unwrap();
        assert_eq!(bytes.len(), 1000);
        assert!(!truncated);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
