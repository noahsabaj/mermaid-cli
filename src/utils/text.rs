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

/// Truncate web content using the default limit, keeping head and tail.
pub fn truncate_web_content(content: &str) -> String {
    truncate_middle(content, WEB_CONTENT_MAX_CHARS)
}

/// Format a duration in seconds as a human-readable string.
///
/// Uses decimal precision for sub-minute durations (e.g., "12.3s"),
/// and integer components for longer durations (e.g., "1m 47s", "2h 5m 0s").
pub fn format_duration(total_secs: f64) -> String {
    let secs = total_secs as u64;
    if secs < 60 {
        return format!("{:.1}s", total_secs);
    }
    let days = secs / 86400;
    let hours = (secs % 86400) / 3600;
    let mins = (secs % 3600) / 60;
    let remainder = secs % 60;
    if days > 0 {
        format!("{}d {}h {}m {}s", days, hours, mins, remainder)
    } else if hours > 0 {
        format!("{}h {}m {}s", hours, mins, remainder)
    } else {
        format!("{}m {}s", mins, remainder)
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
}
