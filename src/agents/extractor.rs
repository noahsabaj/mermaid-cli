/// Block extraction utilities
///
/// Handles extraction of action blocks from AI responses.
/// Supports FILE_WRITE, FILE_READ, COMMAND, and custom block types.
use once_cell::sync::Lazy;
use std::collections::HashMap;

// Cache tag strings to avoid repeated allocations
pub static TAG_CACHE: Lazy<HashMap<&'static str, (String, String)>> = Lazy::new(|| {
    let mut tags = HashMap::new();
    tags.insert(
        "FILE_WRITE",
        (format!("[FILE_WRITE:"), format!("[/FILE_WRITE]")),
    );
    tags.insert(
        "FILE_READ",
        (format!("[FILE_READ:"), format!("[/FILE_READ]")),
    );
    tags.insert("COMMAND", (format!("[COMMAND:"), format!("[/COMMAND]")));
    tags
});

/// Extract blocks of a specific type from the response
pub fn extract_block(text: &str, block_type: &str) -> Option<Vec<String>> {
    // Use cached tags if available, otherwise fall back to format! (for unknown types)
    let (start_tag, end_tag) = TAG_CACHE
        .get(block_type)
        .map(|(s, e)| (s.as_str(), e.as_str()))
        .unwrap_or_else(|| {
            // This should rarely happen since we cache all known types
            (
                Box::leak(format!("[{}:", block_type).into_boxed_str()),
                Box::leak(format!("[/{}]", block_type).into_boxed_str()),
            )
        });

    let mut blocks = Vec::new();
    let mut remaining = text;

    while let Some(start) = remaining.find(start_tag) {
        let block_start = start;
        if let Some(end) = remaining[block_start..].find(end_tag) {
            let block = remaining[block_start..block_start + end + end_tag.len()].to_string();
            blocks.push(block);
            remaining = &remaining[block_start + end + end_tag.len()..];
        } else {
            break;
        }
    }

    if blocks.is_empty() {
        None
    } else {
        Some(blocks)
    }
}

/// Extract content from a block (everything between the tags)
pub fn extract_content(block: &str) -> String {
    if let Some(header_end) = block.find(']') {
        if let Some(footer_start) = block.rfind("[/") {
            return block[header_end + 1..footer_start].trim().to_string();
        }
    }
    String::new()
}

/// Extract path/command from header format [TYPE: path/command]
pub fn extract_path_from_header(block: &str, block_type: &str) -> Option<String> {
    // Use cached start tag
    let start_tag = TAG_CACHE
        .get(block_type)
        .map(|(s, _)| s.as_str())
        .unwrap_or_else(|| Box::leak(format!("[{}:", block_type).into_boxed_str()));

    if let Some(start) = block.find(start_tag) {
        let path_start = start + start_tag.len();
        if let Some(end) = block[path_start..].find(']') {
            let path = block[path_start..path_start + end].trim();
            return Some(path.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_block_single() {
        let response = "[FILE_WRITE: test.rs]\ncode\n[/FILE_WRITE]";
        let blocks = extract_block(response, "FILE_WRITE");
        assert!(blocks.is_some());
        let blocks = blocks.unwrap();
        assert_eq!(blocks.len(), 1);
    }

    #[test]
    fn test_extract_block_multiple() {
        let response = "[CMD: test1]\n[/CMD]\n[CMD: test2]\n[/CMD]";
        let blocks = extract_block(response, "CMD");
        assert!(blocks.is_some());
        let blocks = blocks.unwrap();
        assert_eq!(blocks.len(), 2);
    }

    #[test]
    fn test_extract_block_none() {
        let response = "No blocks here";
        let blocks = extract_block(response, "FILE_WRITE");
        assert!(blocks.is_none());
    }

    #[test]
    fn test_extract_content() {
        let block = "[FILE_WRITE: test.rs]\nfn main() {}\n[/FILE_WRITE]";
        let content = extract_content(block);
        assert!(content.contains("fn main"));
    }

    #[test]
    fn test_extract_path_from_header() {
        let block = "[FILE_READ: src/main.rs]\n[/FILE_READ]";
        let path = extract_path_from_header(block, "FILE_READ");
        assert_eq!(path, Some("src/main.rs".to_string()));
    }
}
