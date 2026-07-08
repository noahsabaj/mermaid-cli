//! Inline `[Image #N]` tokens for the composer.
//!
//! A pasted image becomes an inline text token — `"[Image #N] "` — spliced into
//! the input buffer at the cursor (see `reducer::handle_paste`), backed by an
//! `Attachment` carrying the same global `number`. The token in the text is the
//! source of truth at submit time; `ui.attachments` is just the base64 store
//! keyed by that number. This module is the single home for the token's textual
//! form and for locating/parsing tokens in a buffer, so the reducer's
//! atomic-delete and submit-reconciliation paths never open-code the format.

use std::sync::OnceLock;

use regex::Regex;

/// `[Image #<digits>]` — the canonical pill shape. The trailing space that
/// [`render_token`] appends is deliberately NOT part of the match, so deleting a
/// pill leaves surrounding spacing to the normal editing rules.
fn token_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\[Image #(\d+)\]").expect("valid image-token regex"))
}

/// Inline text inserted at paste time for image `n`, e.g. `"[Image #7] "`. The
/// trailing space lets the user keep typing and makes the pill delete in two
/// predictable keystrokes (space, then pill).
pub fn render_token(n: u64) -> String {
    format!("[Image #{n}] ")
}

/// If a complete `[Image #N]` token ends exactly at byte offset `cursor`, return
/// `(token_start, N)`. Drives atomic Backspace: the whole pill (and its image)
/// go together. A number that overflows `u64` is treated as a non-match (falls
/// back to a normal character delete).
pub fn token_ending_at(buf: &str, cursor: usize) -> Option<(usize, u64)> {
    token_re().captures_iter(buf).find_map(|c| {
        let whole = c.get(0)?;
        if whole.end() != cursor {
            return None;
        }
        let n = c.get(1)?.as_str().parse::<u64>().ok()?;
        Some((whole.start(), n))
    })
}

/// If a complete `[Image #N]` token starts exactly at byte offset `cursor`,
/// return `(token_end, N)`. Symmetric to [`token_ending_at`] for forward-Delete.
pub fn token_starting_at(buf: &str, cursor: usize) -> Option<(usize, u64)> {
    token_re().captures_iter(buf).find_map(|c| {
        let whole = c.get(0)?;
        if whole.start() != cursor {
            return None;
        }
        let n = c.get(1)?.as_str().parse::<u64>().ok()?;
        Some((whole.end(), n))
    })
}

/// Image numbers referenced by `[Image #N]` tokens in `text`, in
/// first-appearance order, de-duplicated. Drives which attachments are sent, and
/// in what order, at submit time. Numbers that overflow `u64` are skipped.
pub fn numbers_in_order(text: &str) -> Vec<u64> {
    let mut out = Vec::new();
    for c in token_re().captures_iter(text) {
        if let Some(n) = c.get(1).and_then(|m| m.as_str().parse::<u64>().ok())
            && !out.contains(&n)
        {
            out.push(n);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_token_has_trailing_space() {
        assert_eq!(render_token(7), "[Image #7] ");
        assert_eq!(render_token(16), "[Image #16] ");
    }

    #[test]
    fn token_ending_at_hits_only_the_exact_boundary() {
        let buf = "see [Image #3] here";
        // "[Image #3]" spans bytes 4..14; it ends at 14.
        assert_eq!(token_ending_at(buf, 14), Some((4, 3)));
        // Off by one either way is a miss.
        assert_eq!(token_ending_at(buf, 13), None);
        assert_eq!(token_ending_at(buf, 15), None);
        // A cursor inside the token is not an ending boundary.
        assert_eq!(token_ending_at(buf, 9), None);
    }

    #[test]
    fn token_ending_at_picks_the_right_one_among_many() {
        let buf = "[Image #1][Image #2]";
        assert_eq!(token_ending_at(buf, 10), Some((0, 1))); // end of first
        assert_eq!(token_ending_at(buf, 20), Some((10, 2))); // end of second
    }

    #[test]
    fn token_starting_at_is_symmetric() {
        let buf = "[Image #2] a [Image #5]";
        assert_eq!(token_starting_at(buf, 0), Some((10, 2)));
        assert_eq!(token_starting_at(buf, 13), Some((23, 5)));
        assert_eq!(token_starting_at(buf, 1), None);
    }

    #[test]
    fn numbers_in_order_dedups_and_preserves_first_appearance() {
        assert_eq!(numbers_in_order("hello"), Vec::<u64>::new());
        assert_eq!(numbers_in_order("[Image #1]"), vec![1]);
        // Out-of-order stays in text order.
        assert_eq!(numbers_in_order("[Image #5] x [Image #3]"), vec![5, 3]);
        // Duplicate collapses to a single entry at first appearance.
        assert_eq!(numbers_in_order("[Image #2] [Image #2]"), vec![2]);
        assert_eq!(
            numbers_in_order("a [Image #7] b [Image #7] c [Image #1]"),
            vec![7, 1]
        );
    }

    #[test]
    fn overflowing_numbers_are_ignored_not_panicked() {
        let huge = "[Image #99999999999999999999999999]"; // > u64::MAX
        assert_eq!(numbers_in_order(huge), Vec::<u64>::new());
        assert_eq!(token_ending_at(huge, huge.len()), None);
    }

    #[test]
    fn non_token_brackets_are_not_matched() {
        assert_eq!(
            numbers_in_order("[Image] [Img #3] [Image #x]"),
            Vec::<u64>::new()
        );
    }
}
