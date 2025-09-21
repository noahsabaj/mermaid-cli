use super::types::AgentAction;

/// Parse actions from AI response text
pub fn parse_actions(response: &str) -> Vec<AgentAction> {
    let mut actions = Vec::new();

    // Parse file write actions
    if let Some(captures) = extract_block(response, "FILE_WRITE") {
        for capture in captures {
            // Extract path from [FILE_WRITE: path] format
            if let Some(path) = extract_path_from_header(&capture, "FILE_WRITE") {
                actions.push(AgentAction::WriteFile {
                    path,
                    content: extract_content(&capture),
                });
            }
        }
    }

    // Parse file read actions
    if let Some(captures) = extract_block(response, "FILE_READ") {
        for capture in captures {
            // Extract path from [FILE_READ: path] format
            if let Some(path) = extract_path_from_header(&capture, "FILE_READ") {
                actions.push(AgentAction::ReadFile { path });
            }
        }
    }

    // Parse command execution
    if let Some(captures) = extract_block(response, "COMMAND") {
        for capture in captures {
            // For COMMAND, the command itself is after the colon
            let command = if let Some(cmd) = extract_path_from_header(&capture, "COMMAND") {
                // Check if there's a dir= attribute
                if let Some(dir_pos) = cmd.find(" dir=") {
                    let command_part = cmd[..dir_pos].to_string();
                    let dir_part = cmd[dir_pos + 5..].trim_matches('"').to_string();
                    actions.push(AgentAction::ExecuteCommand {
                        command: command_part,
                        working_dir: Some(dir_part),
                    });
                } else {
                    actions.push(AgentAction::ExecuteCommand {
                        command: cmd,
                        working_dir: None,
                    });
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

/// Extract blocks of a specific type from the response
fn extract_block(text: &str, block_type: &str) -> Option<Vec<String>> {
    let start_tag = format!("[{}:", block_type);
    let end_tag = format!("[/{}]", block_type);

    let mut blocks = Vec::new();
    let mut remaining = text;

    while let Some(start) = remaining.find(&start_tag) {
        let block_start = start;
        if let Some(end) = remaining[block_start..].find(&end_tag) {
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

/// Extract an attribute from a block header
fn extract_attribute(block: &str, attr: &str) -> Option<String> {
    let pattern = format!("{}=", attr);
    if let Some(start) = block.find(&pattern) {
        let value_start = start + pattern.len();
        let rest = &block[value_start..];

        // Find the end of the attribute value (space, newline, or ])
        let end = rest
            .find(|c: char| c.is_whitespace() || c == ']')
            .unwrap_or(rest.len());

        Some(rest[..end].trim_matches('"').to_string())
    } else {
        None
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
    let start_tag = format!("[{}:", block_type);
    if let Some(start) = block.find(&start_tag) {
        let path_start = start + start_tag.len();
        if let Some(end) = block[path_start..].find(']') {
            let path = block[path_start..path_start + end].trim();
            return Some(path.to_string());
        }
    }
    None
}