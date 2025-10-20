use super::extractor::{extract_block, extract_content, extract_path_from_header};
pub use super::segmenter::{segment_message, MessageSegment};
use super::types::AgentAction;
use crate::utils::{validate_command, validate_file_path};

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
                    },
                    Err(e) => {
                        eprintln!("[SECURITY] Rejected FILE_WRITE: {}", e);
                    },
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
                    },
                    Err(e) => {
                        eprintln!("[SECURITY] Rejected FILE_READ: {}", e);
                    },
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
                                },
                                Err(e) => {
                                    eprintln!(
                                        "[SECURITY] Rejected COMMAND working directory: {}",
                                        e
                                    );
                                },
                            }
                        },
                        Err(e) => {
                            eprintln!("[SECURITY] Rejected COMMAND: {}", e);
                        },
                    }
                } else {
                    match validate_command(&cmd) {
                        Ok(validated_cmd) => {
                            actions.push(AgentAction::ExecuteCommand {
                                command: validated_cmd,
                                working_dir: None,
                            });
                        },
                        Err(e) => {
                            eprintln!("[SECURITY] Rejected COMMAND: {}", e);
                        },
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

    // Parse web search actions
    // Format: [WEB_SEARCH(N): query text here]
    if let Some(web_searches) = parse_web_searches(response) {
        actions.extend(web_searches);
    }

    // Group consecutive same-type actions for parallel execution
    let grouped = group_parallel_actions(actions);
    grouped
}

/// Group consecutive same-type actions for parallel execution
/// Converts sequences like ReadFile, ReadFile, ReadFile -> ParallelRead
fn group_parallel_actions(actions: Vec<AgentAction>) -> Vec<AgentAction> {
    if actions.is_empty() {
        return actions;
    }

    let mut grouped = Vec::new();
    let mut i = 0;

    while i < actions.len() {
        match &actions[i] {
            AgentAction::ReadFile { .. } => {
                // Collect consecutive ReadFile actions
                let mut paths = Vec::new();
                while i < actions.len() {
                    match &actions[i] {
                        AgentAction::ReadFile { path } => {
                            paths.push(path.clone());
                            i += 1;
                        }
                        _ => break,
                    }
                }

                // Group if 2+ files, otherwise keep as individual actions
                if paths.len() >= 2 {
                    grouped.push(AgentAction::ParallelRead { paths });
                } else {
                    for path in paths {
                        grouped.push(AgentAction::ReadFile { path });
                    }
                }
            }
            AgentAction::WebSearch { query, result_count } => {
                // Collect consecutive WebSearch actions
                let mut queries = Vec::new();
                while i < actions.len() {
                    match &actions[i] {
                        AgentAction::WebSearch { query, result_count } => {
                            queries.push((query.clone(), *result_count));
                            i += 1;
                        }
                        _ => break,
                    }
                }

                // Group if 2+ searches, otherwise keep as individual actions
                if queries.len() >= 2 {
                    grouped.push(AgentAction::ParallelWebSearch { queries });
                } else {
                    for (query, result_count) in queries {
                        grouped.push(AgentAction::WebSearch { query, result_count });
                    }
                }
            }
            AgentAction::GitDiff { path } => {
                // Collect consecutive GitDiff actions
                let mut paths = Vec::new();
                while i < actions.len() {
                    match &actions[i] {
                        AgentAction::GitDiff { path } => {
                            paths.push(path.clone());
                            i += 1;
                        }
                        _ => break,
                    }
                }

                // Group if 2+ diffs, otherwise keep as individual actions
                if paths.len() >= 2 {
                    grouped.push(AgentAction::ParallelGitDiff { paths });
                } else {
                    for path in paths {
                        grouped.push(AgentAction::GitDiff { path });
                    }
                }
            }
            _ => {
                // All other action types pass through as-is
                grouped.push(actions[i].clone());
                i += 1;
            }
        }
    }

    grouped
}

/// Parse WEB_SEARCH(N): query format from response
fn parse_web_searches(response: &str) -> Option<Vec<AgentAction>> {
    let mut searches = Vec::new();

    // Match [WEB_SEARCH(N): query] pattern
    // N must be between 1-10
    let mut remaining = response;

    while let Some(start) = remaining.find("[WEB_SEARCH(") {
        // Find the closing parenthesis
        if let Some(paren_end) = remaining[start..].find("):") {
            let count_str = &remaining[start + 12..start + paren_end];

            // Parse count (must be 1-10)
            if let Ok(count) = count_str.parse::<usize>() {
                if count >= 1 && count <= 10 {
                    // Find the closing bracket
                    let search_start = start + paren_end + 2;
                    if let Some(bracket_end) = remaining[search_start..].find(']') {
                        let query = remaining[search_start..search_start + bracket_end].trim().to_string();

                        if !query.is_empty() {
                            searches.push(AgentAction::WebSearch {
                                query,
                                result_count: count,
                            });
                        }

                        // Continue searching after this block
                        remaining = &remaining[search_start + bracket_end..];
                        continue;
                    }
                }
            }
        }

        // Move past this invalid attempt
        remaining = &remaining[start + 1..];
    }

    if searches.is_empty() {
        None
    } else {
        Some(searches)
    }
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

    // Remove WEB_SEARCH markers [WEB_SEARCH(N): query]
    cleaned = remove_web_search_markers(&cleaned);

    // Remove any hallucinated [SEARCH_RESULTS] blocks that the model may have predicted
    cleaned = remove_search_result_blocks(&cleaned);

    // Clean up multiple consecutive newlines that may result from removal
    while cleaned.contains("\n\n\n") {
        cleaned = cleaned.replace("\n\n\n", "\n\n");
    }

    // Trim any leading/trailing whitespace
    cleaned.trim().to_string()
}

/// Remove WEB_SEARCH(N): query markers from text
fn remove_web_search_markers(text: &str) -> String {
    let mut result = text.to_string();
    let mut remaining = text;

    while let Some(start) = remaining.find("[WEB_SEARCH(") {
        if let Some(bracket_end) = remaining[start..].find(']') {
            let marker = &remaining[start..start + bracket_end + 1];
            result = result.replace(marker, "");
            remaining = &remaining[start + bracket_end + 1..];
        } else {
            break;
        }
    }

    result
}

/// Remove hallucinated [SEARCH_RESULTS]...[/SEARCH_RESULTS] blocks
/// These are generated by the model when it tries to predict what search results look like,
/// rather than waiting for actual search results from the feedback loop
fn remove_search_result_blocks(text: &str) -> String {
    let mut result = text.to_string();

    while let Some(start) = result.find("[SEARCH_RESULTS]") {
        if let Some(end) = result[start..].find("[/SEARCH_RESULTS]") {
            let end_pos = start + end + "[/SEARCH_RESULTS]".len();
            result.drain(start..end_pos);
        } else {
            if let Some(pos) = result.find("[SEARCH_RESULTS]") {
                result.drain(pos..pos + "[SEARCH_RESULTS]".len());
            }
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    // File path validation tests
    #[test]
    fn test_validate_file_path_valid_paths() {
        let valid_paths = vec![
            "src/main.rs",
            "appconfig.yaml", // Avoid "config.toml" pattern
            "README.md",
            "src/utils/helper.rs",
            "docs/guide.md",
        ];

        for path in valid_paths {
            let result = validate_file_path(path);
            assert!(result.is_ok(), "Path should be valid: {}", path);
        }
    }

    #[test]
    fn test_validate_file_path_directory_traversal() {
        let invalid_paths = vec![
            "../../etc/passwd",
            "../.ssh/id_rsa",
            "src/../../confidential.txt",
            ".../.config/sensitive",
        ];

        for path in invalid_paths {
            let result = validate_file_path(path);
            assert!(
                result.is_err(),
                "Should reject directory traversal: {}",
                path
            );
            assert!(result.unwrap_err().contains("directory traversal"));
        }
    }

    #[test]
    fn test_validate_file_path_absolute_paths() {
        let absolute_paths = vec![
            "/etc/passwd",
            "/root/.ssh/id_rsa",
            "/home/user/files",
            "/.ssh/config",
        ];

        for path in absolute_paths {
            let result = validate_file_path(path);
            assert!(result.is_err(), "Should reject absolute path: {}", path);
            assert!(result.unwrap_err().contains("Absolute paths not allowed"));
        }
    }

    #[test]
    fn test_validate_file_path_null_bytes() {
        let result = validate_file_path("src/file\0.rs");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("null byte"));
    }

    #[test]
    fn test_validate_file_path_sensitive_files() {
        let sensitive_paths = vec![".ssh/id_rsa", "id_ed25519", ".aws/credentials", ".env"];

        for path in sensitive_paths {
            let result = validate_file_path(path);
            assert!(result.is_err(), "Should reject sensitive file: {}", path);
            let err = result.unwrap_err();
            assert!(err.contains("sensitive") || err.contains("denied"));
        }
    }

    // Command validation tests
    #[test]
    fn test_validate_command_safe_commands() {
        let safe_commands = vec![
            "cargo test",
            "echo hello",
            "ls -la",
            "mkdir new_dir",
            "git status",
        ];

        for cmd in safe_commands {
            let result = validate_command(cmd);
            assert!(result.is_ok(), "Command should be safe: {}", cmd);
        }
    }

    #[test]
    fn test_validate_command_dangerous_keywords() {
        let dangerous = vec![
            "rm -rf /",
            "dd if=/dev/zero",
            "chmod -R 000 /", // Must match exact pattern
        ];

        for cmd in dangerous {
            let result = validate_command(cmd);
            assert!(result.is_err(), "Should block dangerous command: {}", cmd);
        }
    }

    #[test]
    fn test_validate_command_shell_piping() {
        let commands = vec!["command | bash", "something | sh", "eval dangerous_code"];

        for cmd in commands {
            let result = validate_command(cmd);
            assert!(result.is_err(), "Should block shell piping: {}", cmd);
        }
    }

    #[test]
    fn test_validate_command_injection_attempts() {
        let commands = vec!["echo $(whoami)", "echo `id`", "echo $(rm -rf /)"];

        for cmd in commands {
            let result = validate_command(cmd);
            assert!(
                result.is_err(),
                "Should block command substitution: {}",
                cmd
            );
        }
    }

    #[test]
    fn test_validate_command_chaining() {
        let commands = vec!["command1 && command2", "cmd1 && cmd2 && cmd3"];

        for cmd in commands {
            let result = validate_command(cmd);
            assert!(result.is_err(), "Should block command chaining: {}", cmd);
        }
    }

    // Parse actions tests
    #[test]
    fn test_parse_actions_file_write() {
        let response =
            "I will create a file:\n[FILE_WRITE: src/test.rs]\nfn main() {}\n[/FILE_WRITE]";
        let actions = parse_actions(response);

        assert_eq!(actions.len(), 1);
        match &actions[0] {
            AgentAction::WriteFile { path, content } => {
                assert_eq!(path, "src/test.rs");
                assert!(content.contains("fn main"));
            },
            _ => panic!("Expected WriteFile action"),
        }
    }

    #[test]
    fn test_parse_actions_file_read() {
        let response = "Let me read:\n[FILE_READ: src/main.rs]\n[/FILE_READ]";
        let actions = parse_actions(response);

        assert_eq!(actions.len(), 1);
        match &actions[0] {
            AgentAction::ReadFile { path } => {
                assert_eq!(path, "src/main.rs");
            },
            _ => panic!("Expected ReadFile action"),
        }
    }

    #[test]
    fn test_parse_actions_execute_command() {
        let response = "Running:\n[COMMAND: cargo test]\n[/COMMAND]";
        let actions = parse_actions(response);

        assert_eq!(actions.len(), 1);
        match &actions[0] {
            AgentAction::ExecuteCommand {
                command,
                working_dir,
            } => {
                assert_eq!(command, "cargo test");
                assert!(working_dir.is_none());
            },
            _ => panic!("Expected ExecuteCommand action"),
        }
    }

    #[test]
    fn test_parse_actions_command_with_dir() {
        let response = r#"[COMMAND: cargo build dir="src"]\n[/COMMAND]"#;
        let actions = parse_actions(response);

        assert_eq!(actions.len(), 1);
        match &actions[0] {
            AgentAction::ExecuteCommand {
                command,
                working_dir,
            } => {
                assert_eq!(command, "cargo build");
                assert_eq!(working_dir, &Some("src".to_string()));
            },
            _ => panic!("Expected ExecuteCommand with working_dir"),
        }
    }

    #[test]
    fn test_parse_actions_git_operations() {
        let response = "Status:\n[GIT_STATUS]\n\nDiff:\n[GIT_DIFF]";
        let actions = parse_actions(response);

        assert_eq!(actions.len(), 2);
        // Git operations are detected by contains(), not extracted as blocks
        // So order depends on which appears first in text
        assert!(actions.iter().any(|a| matches!(a, AgentAction::GitStatus)));
        assert!(actions
            .iter()
            .any(|a| matches!(a, AgentAction::GitDiff { path: None })));
    }

    #[test]
    fn test_parse_actions_reject_malicious_paths() {
        let response = "[FILE_WRITE: ../../etc/passwd]\nmalicious\n[/FILE_WRITE]";
        let actions = parse_actions(response);

        assert_eq!(actions.len(), 0, "Should reject malicious path");
    }

    #[test]
    fn test_parse_actions_multiple_actions() {
        let response = "[FILE_WRITE: a.txt]\nA\n[/FILE_WRITE]\n[FILE_READ: b.txt]\n[/FILE_READ]\n[COMMAND: test]\n[/COMMAND]";
        let actions = parse_actions(response);

        assert_eq!(actions.len(), 3);
    }

    #[test]
    fn test_strip_action_blocks() {
        let response = "Hello\n[FILE_WRITE: test.rs]\ncode\n[/FILE_WRITE]\nWorld\n[COMMAND: test]\n[/COMMAND]\nEnd";
        let cleaned = strip_action_blocks(response);

        assert!(!cleaned.contains("[FILE_WRITE"));
        assert!(!cleaned.contains("[/FILE_WRITE]"));
        assert!(!cleaned.contains("[COMMAND"));
        assert!(cleaned.contains("Hello"));
        assert!(cleaned.contains("World"));
        assert!(cleaned.contains("End"));
    }

    #[test]
    fn test_strip_action_blocks_git_markers() {
        let response = "Status:\n[GIT_STATUS]\n\nDiff:\n[GIT_DIFF]";
        let cleaned = strip_action_blocks(response);

        assert!(!cleaned.contains("[GIT_STATUS]"));
        assert!(!cleaned.contains("[GIT_DIFF]"));
    }

    #[test]
    fn test_strip_action_blocks_excess_newlines() {
        let response = "Text\n[FILE_WRITE: f.rs]\n[/FILE_WRITE]\n\n\nMore";
        let cleaned = strip_action_blocks(response);

        assert!(!cleaned.contains("\n\n\n"));
    }

    #[test]
    fn test_remove_search_result_blocks() {
        let response = "Here are results:\n[SEARCH_RESULTS]\nTitle: Example\nURL: https://example.com\n[/SEARCH_RESULTS]\nEnd";
        let cleaned = remove_search_result_blocks(response);

        assert!(!cleaned.contains("[SEARCH_RESULTS]"));
        assert!(!cleaned.contains("[/SEARCH_RESULTS]"));
        assert!(cleaned.contains("Here are results:"));
        assert!(cleaned.contains("End"));
    }

    #[test]
    fn test_remove_search_result_blocks_multiple() {
        let response = "[SEARCH_RESULTS]\nResult 1\n[/SEARCH_RESULTS]\nMiddle\n[SEARCH_RESULTS]\nResult 2\n[/SEARCH_RESULTS]";
        let cleaned = remove_search_result_blocks(response);

        assert!(!cleaned.contains("[SEARCH_RESULTS]"));
        assert!(!cleaned.contains("[/SEARCH_RESULTS]"));
        assert!(cleaned.contains("Middle"));
    }

    #[test]
    fn test_remove_search_result_blocks_unclosed() {
        let response = "Start [SEARCH_RESULTS]\nUnclosed result";
        let cleaned = remove_search_result_blocks(response);

        assert!(!cleaned.contains("[SEARCH_RESULTS]"));
        assert!(cleaned.contains("Unclosed result"));
    }

    #[test]
    fn test_strip_action_blocks_with_search_results() {
        let response = "Looking for info:\n[WEB_SEARCH(3): rust async]\nHere are hallucinated results:\n[SEARCH_RESULTS]\nTitle: Fake\n[/SEARCH_RESULTS]\nEnd";
        let cleaned = strip_action_blocks(response);

        assert!(!cleaned.contains("[WEB_SEARCH"));
        assert!(!cleaned.contains("[SEARCH_RESULTS]"));
        assert!(!cleaned.contains("[/SEARCH_RESULTS]"));
        assert!(cleaned.contains("Looking for info:"));
        assert!(cleaned.contains("End"));
    }

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

    #[test]
    fn test_extract_content_from_blocks() {
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
}
