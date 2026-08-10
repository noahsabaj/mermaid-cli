//! @-mention file references: pure token detection + fuzzy ranking.
//!
//! Typing `@` in the composer (at the buffer start or after whitespace)
//! opens a fuzzy file picker over the project's non-ignored files;
//! completing inserts the plain relative path as text (`@src/foo.rs `) —
//! the model reads the file with its own tools, so the mention survives
//! persistence, compaction, replay, and every provider adapter with zero
//! new machinery. File enumeration is an effect (`Cmd::Query(crate::query::Query::ListProjectFiles)`);
//! this module is pure computation over the resulting list.

/// The active `@`-token under the cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AtToken {
    /// Byte offset of the `@` itself.
    pub start: usize,
    /// Byte offset where the query text begins (start + 1).
    pub query_start: usize,
    /// Byte offset where the query ends (== cursor).
    pub query_end: usize,
}

/// Detect an `@`-mention token containing the cursor: an `@` at the buffer
/// start or right after whitespace, with no whitespace between it and the
/// cursor. `user@host` never triggers (the `@` follows a non-space char);
/// UTF-8 boundaries are respected throughout (byte-offset scanning only at
/// char boundaries).
#[must_use]
pub fn active_at_token(buf: &str, cursor: usize) -> Option<AtToken> {
    let cursor = cursor.min(buf.len());
    if !buf.is_char_boundary(cursor) {
        return None;
    }
    // Walk back from the cursor to the nearest `@`; whitespace before an `@`
    // is found means no active token.
    let before = &buf[..cursor];
    let mut start = None;
    for (idx, ch) in before.char_indices().rev() {
        if ch == '@' {
            start = Some(idx);
            break;
        }
        if ch.is_whitespace() {
            return None;
        }
    }
    let start = start?;
    // The `@` must open a token: buffer start or preceded by whitespace.
    if start > 0 {
        let prev = before[..start].chars().next_back()?;
        if !prev.is_whitespace() {
            return None;
        }
    }
    Some(AtToken {
        start,
        query_start: start + 1,
        query_end: cursor,
    })
}

/// Rank `files` against `query`, best first, at most `limit` results.
/// Deterministic: nucleo score descending, input order breaking ties; an
/// empty query returns the lexicographic head of the (pre-sorted) list.
#[must_use]
pub fn fuzzy_rank(files: &[String], query: &str, limit: usize) -> Vec<String> {
    if query.is_empty() {
        return files.iter().take(limit).cloned().collect();
    }
    use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
    use nucleo_matcher::{Config, Matcher};
    let mut matcher = Matcher::new(Config::DEFAULT.match_paths());
    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let mut scored: Vec<(u32, usize)> = Vec::new();
    let mut haystack_buf = Vec::new();
    for (idx, file) in files.iter().enumerate() {
        let haystack = nucleo_matcher::Utf32Str::new(file, &mut haystack_buf);
        if let Some(score) = pattern.score(haystack, &mut matcher) {
            scored.push((score, idx));
        }
    }
    // Score DESC, input order (idx ASC) as the tie-break — fully stable.
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored
        .into_iter()
        .take(limit)
        .map(|(_, idx)| files[idx].clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn token_at_buffer_start() {
        let buf = "@src";
        let tok = active_at_token(buf, 4).expect("token");
        assert_eq!(tok.start, 0);
        assert_eq!(&buf[tok.query_start..tok.query_end], "src");
    }

    #[test]
    fn token_after_whitespace() {
        let buf = "look at @main.rs please";
        // Cursor right after "@main.rs" (byte 16).
        let tok = active_at_token(buf, 16).expect("token");
        assert_eq!(tok.start, 8);
        assert_eq!(&buf[tok.query_start..tok.query_end], "main.rs");
    }

    #[test]
    fn email_like_at_never_triggers() {
        let buf = "mail user@host now";
        // Cursor inside "host".
        assert_eq!(active_at_token(buf, 14), None);
    }

    #[test]
    fn whitespace_between_at_and_cursor_closes_the_token() {
        let buf = "@src and more";
        assert_eq!(active_at_token(buf, 9), None);
    }

    #[test]
    fn cursor_before_the_at_is_not_inside() {
        let buf = "hi @src";
        assert_eq!(active_at_token(buf, 2), None);
    }

    #[test]
    fn multibyte_input_does_not_panic() {
        let buf = "héllo @tê";
        // Cursor at end (a char boundary past multibyte chars).
        let tok = active_at_token(buf, buf.len()).expect("token");
        assert_eq!(&buf[tok.query_start..tok.query_end], "tê");
        // A cursor on a non-boundary byte returns None rather than panicking.
        assert_eq!(active_at_token("é@x", 1), None);
    }

    #[test]
    fn empty_query_returns_lexicographic_head() {
        let list = files(&["a.rs", "b.rs", "c.rs", "d.rs"]);
        assert_eq!(fuzzy_rank(&list, "", 2), files(&["a.rs", "b.rs"]));
    }

    #[test]
    fn ranking_prefers_contiguous_filename_over_scattered_path() {
        let list = files(&["mystic/awesome/insight/notes.md", "src/main.rs"]);
        let ranked = fuzzy_rank(&list, "main", 10);
        assert_eq!(
            ranked[0], "src/main.rs",
            "a contiguous basename match outranks letters scattered across segments: {ranked:?}"
        );
    }

    #[test]
    fn ranking_is_deterministic_across_calls() {
        let list = files(&["alpha.rs", "beta.rs", "src/ab.rs", "docs/ab.md"]);
        let first = fuzzy_rank(&list, "ab", 10);
        let second = fuzzy_rank(&list, "ab", 10);
        assert_eq!(first, second);
        assert!(!first.is_empty());
    }

    #[test]
    fn non_matching_query_yields_empty() {
        let list = files(&["main.rs"]);
        assert!(fuzzy_rank(&list, "zzzqqq", 10).is_empty());
    }
}
