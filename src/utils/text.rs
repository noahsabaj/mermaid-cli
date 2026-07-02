use crate::constants::WEB_CONTENT_MAX_CHARS;

/// Truncate content to a maximum character count (char-boundary safe)
pub fn truncate_content(content: &str, max_chars: usize) -> String {
    // Fast path: if byte length fits, char count definitely fits too
    // (every char is at least 1 byte, so len <= max_chars implies char_count <= max_chars)
    if content.len() <= max_chars {
        return content.to_string();
    }

    // Slow path: multi-byte content might have fewer chars than bytes
    // Find the byte position of the max_chars-th character
    if let Some((byte_end, _)) = content.char_indices().nth(max_chars) {
        format!("{}...[truncated]", &content[..byte_end])
    } else {
        // Fewer than max_chars characters total — no truncation needed
        content.to_string()
    }
}

/// Truncate web content using the default limit
pub fn truncate_web_content(content: &str) -> String {
    truncate_content(content, WEB_CONTENT_MAX_CHARS)
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
    fn test_truncate_content() {
        let short = "hello";
        assert_eq!(truncate_content(short, 100), "hello");

        let long = "a".repeat(200);
        let truncated = truncate_content(&long, 50);
        assert!(truncated.ends_with("...[truncated]"));
        assert!(truncated.len() < 200);
    }
}
