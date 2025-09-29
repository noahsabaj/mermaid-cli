use super::types::AgentAction;
use crate::constants::DANGEROUS_COMMANDS;
use std::path::Path;

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
