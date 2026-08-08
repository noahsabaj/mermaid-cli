use crate::constants::WEB_CONTENT_MAX_CHARS;

/// Truncate content to a maximum character count, keeping the HEAD (char-boundary
/// safe). Prefer [`truncate_middle`] where the tail matters (command/tool output);
/// this remains for per-item web caps where head-only is acceptable.
pub fn truncate_content(content: &str, max_chars: usize) -> String {
    if content.len() <= max_chars {
        return content.to_string();
    }
    if let Some((byte_end, _)) = content.char_indices().nth(max_chars) {
        format!("{}...[truncated]", &content[..byte_end])
    } else {
        content.to_string()
    }
}

/// Truncate `content` to about `max_chars` characters, keeping the HEAD and the
/// TAIL with an elision marker in the middle (char-boundary safe). Command/tool
/// output and web pages put the most important content — compiler errors, exit
/// summaries, page footers — at the END, so head-only truncation discarded
/// exactly what mattered. Content that already fits is returned unchanged.
pub fn truncate_middle(content: &str, max_chars: usize) -> String {
    // Fast path: fits by bytes ⇒ fits by chars (every char is ≥ 1 byte).
    if content.len() <= max_chars {
        return content.to_string();
    }
    let total_chars = content.chars().count();
    if total_chars <= max_chars {
        return content.to_string();
    }
    let head_chars = max_chars / 2;
    let tail_chars = max_chars - head_chars;
    let elided = total_chars - head_chars - tail_chars;
    let head_end = content
        .char_indices()
        .nth(head_chars)
        .map(|(i, _)| i)
        .unwrap_or(content.len());
    let tail_start = content
        .char_indices()
        .nth(total_chars - tail_chars)
        .map(|(i, _)| i)
        .unwrap_or(content.len());
    format!(
        "{}\n…[{elided} chars elided]…\n{}",
        &content[..head_end],
        &content[tail_start..]
    )
}

/// Truncate to an exact UTF-8 byte budget while preserving both ends.
///
/// This differs from [`truncate_middle`], whose budget is measured in Unicode
/// scalar values. Protocol envelopes and tool-result limits are byte budgets,
/// so multi-byte text must be cut on a character boundary without exceeding
/// `max_bytes` after the marker is included.
pub fn truncate_middle_bytes(content: &str, max_bytes: usize) -> String {
    if content.len() <= max_bytes {
        return content.to_string();
    }

    const MARKER: &str = "\n...[content truncated]...\n";
    if max_bytes <= MARKER.len() {
        let end = content.floor_char_boundary(max_bytes);
        return content[..end].to_string();
    }

    let keep = max_bytes - MARKER.len();
    let head_budget = keep / 2;
    let tail_budget = keep - head_budget;
    let head_end = content.floor_char_boundary(head_budget);
    let mut tail_start = content.len().saturating_sub(tail_budget);
    while tail_start < content.len() && !content.is_char_boundary(tail_start) {
        tail_start += 1;
    }

    format!("{}{MARKER}{}", &content[..head_end], &content[tail_start..])
}

/// Truncate web content using the default limit, keeping head and tail.
pub fn truncate_web_content(content: &str) -> String {
    truncate_middle(content, WEB_CONTENT_MAX_CHARS)
}

/// How far back into the previous chunk's tail to search for an echo. Bounds
/// the cost of [`continuation_overlap`] and keeps a coincidental match deep
/// inside the previous text from being mistaken for a resume-echo.
const CONTINUATION_OVERLAP_WINDOW_BYTES: usize = 400;
/// Minimum echo length worth trimming. Short exact matches ("the ", "- ", a
/// repeated word) are as likely to be legitimate new prose as an echo, and
/// trimming a false positive silently deletes real output — so anything under
/// this threshold is kept verbatim.
const CONTINUATION_OVERLAP_MIN_BYTES: usize = 16;

/// Length in bytes of the longest exact overlap between the tail of `prev`
/// and the head of `continuation` — the "resume echo" a model sometimes emits
/// when continuing a reply that was cut by a per-response output cap.
///
/// Deliberately conservative: exact match only, bounded search window,
/// minimum overlap threshold. Used for DISPLAY/output joining only — the
/// canonical conversation history is never trimmed — so a false negative
/// costs a few repeated words on screen, while a false positive would delete
/// real content. Returns a char-boundary-safe byte offset into `continuation`.
pub fn continuation_overlap(prev: &str, continuation: &str) -> usize {
    // Window into prev's tail, aligned to a char boundary.
    let mut window_start = prev.len().saturating_sub(CONTINUATION_OVERLAP_WINDOW_BYTES);
    while window_start < prev.len() && !prev.is_char_boundary(window_start) {
        window_start += 1;
    }
    let tail = &prev[window_start..];
    let max_len = tail.len().min(continuation.len());
    if max_len < CONTINUATION_OVERLAP_MIN_BYTES {
        return 0;
    }
    // Longest suffix-of-prev == prefix-of-continuation, on char boundaries.
    for len in (CONTINUATION_OVERLAP_MIN_BYTES..=max_len).rev() {
        if !continuation.is_char_boundary(len) {
            continue;
        }
        let head = &continuation[..len];
        if tail.ends_with(head) {
            return len;
        }
    }
    0
}

/// Format a duration in seconds as a human-readable string.
///
/// Uses decimal precision for sub-minute durations (e.g., "12.3s"),
/// and integer components for longer durations (e.g., "1m 47s", "2h 5m 0s").
pub fn format_duration(total_secs: f64) -> String {
    let secs = total_secs as u64;
    if secs < 60 {
        return format!("{total_secs:.1}s");
    }
    let days = secs / 86400;
    let hours = (secs % 86400) / 3600;
    let mins = (secs % 3600) / 60;
    let remainder = secs % 60;
    if days > 0 {
        format!("{days}d {hours}h {mins}m {remainder}s")
    } else if hours > 0 {
        format!("{hours}h {mins}m {remainder}s")
    } else {
        format!("{mins}m {remainder}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_duration_sub_minute() {
        assert_eq!(format_duration(0.0), "0.0s");
        assert_eq!(format_duration(12.3), "12.3s");
        assert_eq!(format_duration(59.9), "59.9s");
    }

    #[test]
    fn test_format_duration_minutes_and_above() {
        assert_eq!(format_duration(60.0), "1m 0s");
        assert_eq!(format_duration(107.0), "1m 47s");
        assert_eq!(format_duration(3600.0), "1h 0m 0s");
        assert_eq!(format_duration(86400.0), "1d 0h 0m 0s");
        assert_eq!(format_duration(90061.0), "1d 1h 1m 1s");
    }

    #[test]
    fn continuation_overlap_trims_a_resume_echo() {
        // The model repeats the tail of the cut reply before continuing.
        let prev = "The resolver clamps the budget to the window room";
        let cont = "to the window room, then omits the field entirely.";
        assert_eq!(continuation_overlap(prev, cont), "to the window room".len());
    }

    #[test]
    fn continuation_overlap_keeps_short_ambiguous_matches() {
        // "the " is as likely legitimate prose as an echo — below the minimum
        // threshold nothing is trimmed (a false trim deletes real output).
        assert_eq!(
            continuation_overlap("…and then the ", "the answer is 42"),
            0
        );
        // No overlap at all.
        assert_eq!(continuation_overlap("first half", "second half"), 0);
        // Empty inputs.
        assert_eq!(continuation_overlap("", "anything"), 0);
        assert_eq!(continuation_overlap("anything", ""), 0);
    }

    #[test]
    fn continuation_overlap_prefers_the_longest_echo() {
        // Both "cap. " and the full sentence match; take the longest.
        let prev = "It hit the cap. It hit the cap. ";
        let cont = "It hit the cap. Continuing now.";
        assert_eq!(continuation_overlap(prev, cont), "It hit the cap. ".len());
    }

    #[test]
    fn continuation_overlap_is_window_bounded() {
        // An echo of text further back than the search window is not found —
        // deep coincidental matches must not trigger trimming.
        let echo = "a distinctive sentence that repeats";
        let prev = format!("{echo}{}", "x".repeat(500));
        assert_eq!(continuation_overlap(&prev, echo), 0);
    }

    #[test]
    fn continuation_overlap_respects_char_boundaries() {
        // Multi-byte content: the returned offset must be sliceable.
        let prev = "código con acentuación específica";
        let cont = "acentuación específica y más contenido";
        let n = continuation_overlap(prev, cont);
        assert_eq!(&cont[..n], "acentuación específica");
        let _ = &cont[n..]; // must not panic
    }

    #[test]
    fn truncate_middle_keeps_head_and_tail() {
        let short = "hello";
        assert_eq!(truncate_middle(short, 100), "hello");

        // 200 'H's + a distinctive tail; truncating to 50 must keep BOTH ends.
        let long = format!("{}TAIL_ERROR", "H".repeat(200));
        let truncated = truncate_middle(&long, 50);
        assert!(
            truncated.starts_with("HHHH"),
            "head must survive: {truncated}"
        );
        assert!(
            truncated.ends_with("TAIL_ERROR"),
            "tail must survive: {truncated}"
        );
        assert!(
            truncated.contains("elided"),
            "must mark elision: {truncated}"
        );
        assert!(truncated.chars().count() < long.chars().count());
    }

    #[test]
    fn truncate_middle_bytes_never_exceeds_utf8_budget() {
        for unit in ["a", "é", "界"] {
            let input = unit.repeat(40_000);
            for budget in [0, 1, 8, 29, 30_000] {
                let output = truncate_middle_bytes(&input, budget);
                assert!(output.len() <= budget, "{} > {budget}", output.len());
                assert!(std::str::from_utf8(output.as_bytes()).is_ok());
            }
        }
    }
}
