//! Diff-line marker conventions shared between the edit_file tool
//! (which generates diff output) and the chat renderer (which colors
//! added/removed lines red/green).
//!
//! Kept in the render layer rather than in the tool impl because
//! every consumer is the renderer. The producer today is
//! `generate_diff` — inside the edit tool's helper code — and uses
//! the same constants to format its lines.

/// Marker for a REMOVED line. Formatted as `{num:>4}{marker}{content}`
/// so the three-byte width stays aligned across line numbers up to
/// 9,999. Lines without a matching prefix fall through to `Context`.
pub const DIFF_REMOVED_MARKER: &str = " - ";

/// Marker for an ADDED line.
pub const DIFF_ADDED_MARKER: &str = " + ";

/// Classification of a diff line. The chat renderer matches on this
/// to choose a background color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    Context,
    Removed,
    Added,
}

/// Parse one line of diff output. Lines that don't follow the
/// `{number}{marker}{content}` shape — including malformed or
/// truncated input — fall through to `Context` so the renderer's
/// match stays exhaustive without panicking.
pub fn parse_diff_line(line: &str) -> DiffLineKind {
    let trimmed = line.trim_start();
    let after_num = trimmed.trim_start_matches(|c: char| c.is_ascii_digit());
    if after_num.starts_with(DIFF_REMOVED_MARKER) {
        DiffLineKind::Removed
    } else if after_num.starts_with(DIFF_ADDED_MARKER) {
        DiffLineKind::Added
    } else {
        DiffLineKind::Context
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_added_line() {
        let line = format!("   5{}fn main() {{", DIFF_ADDED_MARKER);
        assert_eq!(parse_diff_line(&line), DiffLineKind::Added);
    }

    #[test]
    fn parses_removed_line() {
        let line = format!("  12{}old = true;", DIFF_REMOVED_MARKER);
        assert_eq!(parse_diff_line(&line), DiffLineKind::Removed);
    }

    #[test]
    fn parses_context_line() {
        let line = "   7   existing line";
        assert_eq!(parse_diff_line(line), DiffLineKind::Context);
    }

    #[test]
    fn malformed_falls_through_to_context() {
        assert_eq!(parse_diff_line("no number at start"), DiffLineKind::Context);
        assert_eq!(parse_diff_line(""), DiffLineKind::Context);
    }
}
