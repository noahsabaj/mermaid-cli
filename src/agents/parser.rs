use super::types::AgentAction;
use crate::constants::DANGEROUS_COMMANDS;
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::path::Path;

// Cache tag strings to avoid repeated allocations
static TAG_CACHE: Lazy<HashMap<&'static str, (String, String)>> = Lazy::new(|| {
    let mut tags = HashMap::new();
    tags.insert("FILE_WRITE", (format!("[FILE_WRITE:"), format!("[/FILE_WRITE]")));
    tags.insert("FILE_READ", (format!("[FILE_READ:"), format!("[/FILE_READ]")));
    tags.insert("COMMAND", (format!("[COMMAND:"), format!("[/COMMAND]")));
    tags
});

/// Validate file path to prevent directory traversal attacks
fn validate_file_path(path: &str) -> Result<String, String> {
    // Reject paths with directory traversal attempts
    if path.contains("..") {
        return Err(format!("Path contains directory traversal: {}", path));
    }

    // Reject absolute paths outside project
    let path_obj = Path::new(path);
    if path_obj.is_absolute() {
        return Err(format!("Absolute paths not allowed: {}", path));
    }

    // Reject paths with null bytes
    if path.contains('\0') {
        return Err("Path contains null byte".to_string());
    }

    // Reject paths targeting sensitive files
    let sensitive_patterns = [
        ".ssh/", "id_rsa", "id_dsa", "id_ecdsa", "id_ed25519",
        ".aws/", "credentials", ".env", "config.toml",
        "/etc/passwd", "/etc/shadow",
    ];

    for pattern in &sensitive_patterns {
        if path.contains(pattern) {
            return Err(format!("Access to sensitive file denied: {}", path));
        }
    }

    Ok(path.to_string())
}

/// Validate command to prevent dangerous operations
fn validate_command(command: &str) -> Result<String, String> {
    // Check against known dangerous commands
    for dangerous in DANGEROUS_COMMANDS {
        if command.contains(dangerous) {
            return Err(format!("Dangerous command blocked: {}", dangerous));
        }
    }

    // Additional checks for pipe to bash/sh
    if (command.contains('|') && (command.contains("bash") || command.contains("sh")))
        || command.contains("eval")
    {
        return Err("Piping to shell interpreter is not allowed".to_string());
    }

    // Check for command injection attempts (more precise matching)
    if command.contains("$(") || command.contains("`") {
        return Err("Command substitution detected".to_string());
    }

    // Check for command chaining (but allow it in some contexts)
    // Block && only if it's standalone (not part of a word)
    if command.contains(" && ") || command.ends_with("&&") || command.starts_with("&&") {
        return Err("Command chaining detected".to_string());
    }

    Ok(command.to_string())
}

/// Parse actions from AI response text
pub fn parse_actions(response: &str) -> Vec<AgentAction> {
    let mut actions = Vec::new();

    // Parse file write actions
    if let Some(captures) = extract_block(response, "FILE_WRITE") {
        for capture in captures {
            // Extract path from [FILE_WRITE: path] format
            if let Some(path) = extract_path_from_header(&capture, "FILE_WRITE") {
                match validate_file_path(&path) {
                    Ok(validated_path) => {
                        actions.push(AgentAction::WriteFile {
                            path: validated_path,
                            content: extract_content(&capture),
                        });
                    }
                    Err(e) => {
                        eprintln!("[SECURITY] Rejected FILE_WRITE: {}", e);
                    }
                }
            }
        }
    }

    // Parse file read actions
    if let Some(captures) = extract_block(response, "FILE_READ") {
        for capture in captures {
            // Extract path from [FILE_READ: path] format
            if let Some(path) = extract_path_from_header(&capture, "FILE_READ") {
                match validate_file_path(&path) {
                    Ok(validated_path) => {
                        actions.push(AgentAction::ReadFile {
                            path: validated_path,
                        });
                    }
                    Err(e) => {
                        eprintln!("[SECURITY] Rejected FILE_READ: {}", e);
                    }
                }
            }
        }
    }

    // Parse command execution
    if let Some(captures) = extract_block(response, "COMMAND") {
        for capture in captures {
            // For COMMAND, the command itself is after the colon
            if let Some(cmd) = extract_path_from_header(&capture, "COMMAND") {
                // Check if there's a dir= attribute
                if let Some(dir_pos) = cmd.find(" dir=") {
                    let command_part = cmd[..dir_pos].to_string();
                    let dir_part = cmd[dir_pos + 5..].trim_matches('"').to_string();

                    // Validate command
                    match validate_command(&command_part) {
                        Ok(validated_cmd) => {
                            // Validate directory path
                            match validate_file_path(&dir_part) {
                                Ok(validated_dir) => {
                                    actions.push(AgentAction::ExecuteCommand {
                                        command: validated_cmd,
                                        working_dir: Some(validated_dir),
                                    });
                                }
                                Err(e) => {
                                    eprintln!("[SECURITY] Rejected COMMAND working directory: {}", e);
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!("[SECURITY] Rejected COMMAND: {}", e);
                        }
                    }
                } else {
                    match validate_command(&cmd) {
                        Ok(validated_cmd) => {
                            actions.push(AgentAction::ExecuteCommand {
                                command: validated_cmd,
                                working_dir: None,
                            });
                        }
                        Err(e) => {
                            eprintln!("[SECURITY] Rejected COMMAND: {}", e);
                        }
                    }
                }
            }
        }
    }

    // Parse git operations
    if response.contains("[GIT_DIFF]") {
        actions.push(AgentAction::GitDiff { path: None });
    }

    if response.contains("[GIT_STATUS]") {
        actions.push(AgentAction::GitStatus);
    }

    actions
}

/// Strip all action blocks from response text for display
/// Returns clean text with action blocks removed
pub fn strip_action_blocks(response: &str) -> String {
    let mut cleaned = response.to_string();

    // Remove all known block types
    for block_type in ["FILE_WRITE", "FILE_READ", "COMMAND"] {
        if let Some(blocks) = extract_block(&cleaned, block_type) {
            for block in blocks {
                cleaned = cleaned.replace(&block, "");
            }
        }
    }

    // Remove git operation markers
    cleaned = cleaned.replace("[GIT_DIFF]", "");
    cleaned = cleaned.replace("[GIT_STATUS]", "");

    // Clean up multiple consecutive newlines that may result from removal
    while cleaned.contains("\n\n\n") {
        cleaned = cleaned.replace("\n\n\n", "\n\n");
    }

    // Trim any leading/trailing whitespace
    cleaned.trim().to_string()
}

/// Extract blocks of a specific type from the response
fn extract_block(text: &str, block_type: &str) -> Option<Vec<String>> {
    // Use cached tags if available, otherwise fall back to format! (for unknown types)
    let (start_tag, end_tag) = TAG_CACHE
        .get(block_type)
        .map(|(s, e)| (s.as_str(), e.as_str()))
        .unwrap_or_else(|| {
            // This should rarely happen since we cache all known types
            (Box::leak(format!("[{}:", block_type).into_boxed_str()),
             Box::leak(format!("[/{}]", block_type).into_boxed_str()))
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
fn extract_content(block: &str) -> String {
    if let Some(header_end) = block.find(']') {
        if let Some(footer_start) = block.rfind("[/") {
            return block[header_end + 1..footer_start].trim().to_string();
        }
    }
    String::new()
}

/// Extract path/command from header format [TYPE: path/command]
fn extract_path_from_header(block: &str, block_type: &str) -> Option<String> {
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

/// Represents a segment of a message (either text or action marker)
#[derive(Debug, Clone)]
pub enum MessageSegment {
    Text(String),
    ActionMarker {
        action_type: String,
        target: String,
    },
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
