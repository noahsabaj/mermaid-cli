use super::types::AgentAction;

/// Parse actions from AI response text
pub fn parse_actions(response: &str) -> Vec<AgentAction> {
    let mut actions = Vec::new();

    // Parse file write actions
    if let Some(captures) = extract_block(response, "FILE_WRITE") {
        for capture in captures {
            if let Some(path) = extract_attribute(&capture, "path") {
                actions.push(AgentAction::WriteFile {
                    path: path.clone(),
                    content: extract_content(&capture),
                });
            }
        }
    }

    // Parse file read actions
    if let Some(captures) = extract_block(response, "FILE_READ") {
        for capture in captures {
            if let Some(path) = extract_attribute(&capture, "path") {
                actions.push(AgentAction::ReadFile { path });
            }
        }
    }

    // Parse command execution
    if let Some(captures) = extract_block(response, "COMMAND") {
        for capture in captures {
            actions.push(AgentAction::ExecuteCommand {
                command: extract_content(&capture),
                working_dir: extract_attribute(&capture, "dir"),
            });
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