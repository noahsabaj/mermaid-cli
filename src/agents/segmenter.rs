/// Message segmentation logic
///
/// Segments a message into alternating text and action markers
/// for interleaved rendering.
use crate::agents::extractor::{extract_block, extract_path_from_header};

/// Represents a segment of a message (either text or action marker)
#[derive(Debug, Clone)]
pub enum MessageSegment {
    Text(String),
    ActionMarker { action_type: String, target: String },
}

/// Segment a message into alternating text and action markers
/// This allows for interleaved rendering where actions appear at their natural positions
pub fn segment_message(content: &str) -> Vec<MessageSegment> {
    let mut segments = Vec::new();

    // Find all action blocks and their positions
    let mut action_positions: Vec<(usize, usize, String, String)> = Vec::new();

    // Find all blocks of each type
    for block_type in ["FILE_WRITE", "FILE_READ", "COMMAND"] {
        if let Some(blocks) = extract_block(content, block_type) {
            let mut search_start = 0;
            for block in blocks {
                // Find this block's position starting from where we last found one
                if let Some(relative_pos) = content[search_start..].find(&block) {
                    let absolute_pos = search_start + relative_pos;
                    if let Some(target) = extract_path_from_header(&block, block_type) {
                        let action_type = match block_type {
                            "FILE_WRITE" => "Write",
                            "FILE_READ" => "Read",
                            "COMMAND" => "Bash",
                            _ => block_type,
                        };
                        action_positions.push((
                            absolute_pos,
                            absolute_pos + block.len(),
                            action_type.to_string(),
                            target,
                        ));
                        // Move search position past this block
                        search_start = absolute_pos + block.len();
                    }
                }
            }
        }
    }

    // Sort by position
    action_positions.sort_by_key(|(start, _, _, _)| *start);

    // Build segments
    let mut last_end = 0;
    for (start, end, action_type, target) in action_positions {
        // Add text segment before this action
        if start > last_end {
            let text = content[last_end..start].to_string();
            if !text.trim().is_empty() {
                segments.push(MessageSegment::Text(text));
            }
        }

        // Add action marker
        segments.push(MessageSegment::ActionMarker {
            action_type,
            target,
        });

        last_end = end;
    }

    // Add remaining text after last action
    if last_end < content.len() {
        let text = content[last_end..].to_string();
        if !text.trim().is_empty() {
            segments.push(MessageSegment::Text(text));
        }
    }

    // If no actions found, return entire content as single text segment
    if segments.is_empty() {
        segments.push(MessageSegment::Text(content.to_string()));
    }

    segments
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_segment_message_no_actions() {
        let content = "Just plain text with no actions";
        let segments = segment_message(content);

        assert_eq!(segments.len(), 1);
        match &segments[0] {
            MessageSegment::Text(text) => assert_eq!(text, content),
            _ => panic!("Expected text segment"),
        }
    }

    #[test]
    fn test_segment_message_with_actions() {
        let content = "Start [FILE_WRITE: test.rs]\ncode\n[/FILE_WRITE] Middle [FILE_READ: data.json]\n[/FILE_READ] End";
        let segments = segment_message(content);

        assert!(segments.len() >= 3);
        assert!(segments
            .iter()
            .any(|s| matches!(s, MessageSegment::Text(_))));
        assert!(segments
            .iter()
            .any(|s| matches!(s, MessageSegment::ActionMarker { .. })));
    }

    #[test]
    fn test_segment_message_action_types() {
        let content = "[FILE_WRITE: a.txt]\n[/FILE_WRITE] [FILE_READ: b.txt]\n[/FILE_READ] [COMMAND: cmd]\n[/COMMAND]";
        let segments = segment_message(content);

        let action_types: Vec<String> = segments
            .iter()
            .filter_map(|s| match s {
                MessageSegment::ActionMarker {
                    action_type,
                    target: _,
                } => Some(action_type.clone()),
                _ => None,
            })
            .collect();

        assert!(action_types.contains(&"Write".to_string()));
        assert!(action_types.contains(&"Read".to_string()));
        assert!(action_types.contains(&"Bash".to_string()));
    }
}
