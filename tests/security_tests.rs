// Integration tests for security features

use mermaid_cli::agents::parse_actions;

#[test]
fn test_directory_traversal_blocked() {
    let response = "[FILE_WRITE: ../../etc/passwd]\ncontent\n[/FILE_WRITE]";
    let actions = parse_actions(response);
    // Should be empty because security validation blocked it
    assert_eq!(actions.len(), 0, "Directory traversal should be blocked");
}

#[test]
fn test_null_byte_injection_blocked() {
    let response = "[FILE_READ: file\x00.txt]";
    let actions = parse_actions(response);
    assert_eq!(actions.len(), 0, "Null byte injection should be blocked");
}

#[test]
fn test_dangerous_command_blocked() {
    let response = "[COMMAND: rm -rf /]\n[/COMMAND]";
    let actions = parse_actions(response);
    assert_eq!(actions.len(), 0, "Dangerous command should be blocked");
}

#[test]
fn test_command_injection_blocked() {
    let response = "[COMMAND: ls && rm -rf /]\n[/COMMAND]";
    let actions = parse_actions(response);
    assert_eq!(actions.len(), 0, "Command injection should be blocked");
}

#[test]
fn test_pipe_to_shell_blocked() {
    let response = "[COMMAND: cat file | bash]\n[/COMMAND]";
    let actions = parse_actions(response);
    assert_eq!(actions.len(), 0, "Pipe to shell should be blocked");
}

#[test]
fn test_sensitive_file_access_blocked() {
    let test_cases = vec![
        "[FILE_READ: .ssh/id_rsa]",
        "[FILE_READ: ~/.aws/credentials]",
        "[FILE_WRITE: .env]\ncontent\n[/FILE_WRITE]",
    ];

    for response in test_cases {
        let actions = parse_actions(response);
        assert_eq!(
            actions.len(),
            0,
            "Sensitive file access should be blocked: {}",
            response
        );
    }
}

#[test]
fn test_valid_file_operations_allowed() {
    let response = "[FILE_WRITE: src/main.rs]\nfn main() {}\n[/FILE_WRITE]";
    let actions = parse_actions(response);
    assert_eq!(actions.len(), 1, "Valid file operations should be allowed");
}

#[test]
fn test_valid_commands_allowed() {
    // Use simpler commands that won't trigger any heuristics
    let response = "[COMMAND: ls]\n[/COMMAND]";
    let actions = parse_actions(response);
    assert_eq!(actions.len(), 1, "Simple safe commands should be allowed");
}
