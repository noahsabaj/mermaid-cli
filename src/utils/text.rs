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

/// Format token count for display: "X.Xk" for >= 1000, raw number otherwise.
pub fn format_tokens(tokens: usize) -> String {
    if tokens >= 1000 {
        format!("{:.1}k tokens", tokens as f64 / 1000.0)
    } else {
        format!("{} tokens", tokens)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
