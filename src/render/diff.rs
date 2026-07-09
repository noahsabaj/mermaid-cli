//! Diff rendering shared between the file-mutating tools (`write_file`,
//! `apply_patch`) that PRODUCE diff output and the chat renderer that COLORS
//! added/removed lines. Both the marker conventions and the producer
//! ([`generate_display_diff`]) live here in the render layer, so every tool that
//! needs a display diff shares one implementation and one format.

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

/// A rendered display diff plus its change counts. Produced by
/// [`generate_display_diff`] and carried on tool metadata for the renderer.
#[derive(Debug, Clone)]
pub(crate) struct DisplayDiff {
    pub display_diff: String,
    pub added: usize,
    pub removed: usize,
    pub truncated: bool,
}

/// Lines of unchanged context shown around a change.
const DIFF_CONTEXT_LINES: usize = 3;
/// Hard cap on rendered diff lines so a huge edit can't flood the transcript.
pub(crate) const MAX_DISPLAY_DIFF_LINES: usize = 220;

/// Render a line-numbered display diff of `old` → `new`. Emits only changed
/// lines plus a few lines of surrounding context, each as
/// `{num:>4}{marker}{content}` using the shared markers so [`parse_diff_line`]
/// colors it. No unified-diff header lines. Shared by `write_file` and
/// `apply_patch`.
pub(crate) fn generate_display_diff(old: &str, new: &str) -> DisplayDiff {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let mut prefix = 0usize;
    let min_len = old_lines.len().min(new_lines.len());
    while prefix < min_len && old_lines[prefix] == new_lines[prefix] {
        prefix += 1;
    }

    let mut suffix = 0usize;
    while suffix < min_len.saturating_sub(prefix)
        && old_lines[old_lines.len() - 1 - suffix] == new_lines[new_lines.len() - 1 - suffix]
    {
        suffix += 1;
    }

    let old_changed_end = old_lines.len().saturating_sub(suffix);
    let new_changed_end = new_lines.len().saturating_sub(suffix);
    let old_changed = &old_lines[prefix..old_changed_end];
    let new_changed = &new_lines[prefix..new_changed_end];
    let added = new_changed.len();
    let removed = old_changed.len();

    let context_start = prefix.saturating_sub(DIFF_CONTEXT_LINES);
    let context_end_old = (old_changed_end + DIFF_CONTEXT_LINES).min(old_lines.len());
    let mut lines = Vec::new();

    let mut truncated = false;
    let push_line = |line: String, lines: &mut Vec<String>, truncated: &mut bool| {
        if lines.len() < MAX_DISPLAY_DIFF_LINES {
            lines.push(line);
        } else {
            *truncated = true;
        }
    };

    for (idx, line) in old_lines[context_start..prefix].iter().enumerate() {
        push_line(
            format!("{:>4}   {}", context_start + idx + 1, line),
            &mut lines,
            &mut truncated,
        );
    }
    for (idx, line) in old_changed.iter().enumerate() {
        push_line(
            format!("{:>4}{}{}", prefix + idx + 1, DIFF_REMOVED_MARKER, line),
            &mut lines,
            &mut truncated,
        );
    }
    for (idx, line) in new_changed.iter().enumerate() {
        push_line(
            format!("{:>4}{}{}", prefix + idx + 1, DIFF_ADDED_MARKER, line),
            &mut lines,
            &mut truncated,
        );
    }
    for (idx, line) in old_lines[old_changed_end..context_end_old]
        .iter()
        .enumerate()
    {
        push_line(
            format!("{:>4}   {}", old_changed_end + idx + 1, line),
            &mut lines,
            &mut truncated,
        );
    }
    if truncated {
        lines.push(format!(
            "... diff truncated after {MAX_DISPLAY_DIFF_LINES} display lines"
        ));
    }

    DisplayDiff {
        display_diff: lines.join("\n"),
        added,
        removed,
        truncated,
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
