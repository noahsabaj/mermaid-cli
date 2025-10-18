/// Security validation utilities for file paths and commands
///
/// This module provides centralized validation functions to prevent common
/// security issues like directory traversal and command injection attacks.
use crate::constants::DANGEROUS_COMMANDS;
use std::path::Path;

/// Validate file path to prevent directory traversal attacks
///
/// # Checks
/// - Rejects paths with `..` (directory traversal attempts)
/// - Rejects absolute paths outside the project
/// - Rejects paths with null bytes
/// - Rejects paths targeting sensitive files (.ssh, .aws, .env, etc.)
///
/// # Arguments
/// * `path` - The file path to validate
///
/// # Returns
/// * `Ok(path)` - The validated path if all checks pass
/// * `Err(reason)` - A string describing why the path was rejected
pub fn validate_file_path(path: &str) -> Result<String, String> {
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
        ".ssh/",
        "id_rsa",
        "id_dsa",
        "id_ecdsa",
        "id_ed25519",
        ".aws/",
        "credentials",
        ".env",
        "config.toml",
        "/etc/passwd",
        "/etc/shadow",
    ];

    for pattern in &sensitive_patterns {
        if path.contains(pattern) {
            return Err(format!("Access to sensitive file denied: {}", path));
        }
    }

    Ok(path.to_string())
}

/// Validate command to prevent dangerous operations and command injection
///
/// # Checks
/// - Rejects known dangerous commands from DANGEROUS_COMMANDS list
/// - Rejects piping to bash/sh shells
/// - Rejects command substitution attempts (`$(...)` or backticks)
/// - Rejects command chaining (`&&`)
/// - Rejects `eval` command
///
/// # Arguments
/// * `command` - The command string to validate
///
/// # Returns
/// * `Ok(command)` - The validated command if all checks pass
/// * `Err(reason)` - A string describing why the command was rejected
pub fn validate_command(command: &str) -> Result<String, String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_file_path_directory_traversal() {
        assert!(validate_file_path("../secret.txt").is_err());
        assert!(validate_file_path("../../etc/passwd").is_err());
    }

    #[test]
    fn test_validate_file_path_absolute_paths() {
        assert!(validate_file_path("/etc/passwd").is_err());
        assert!(validate_file_path("/home/user/.ssh/id_rsa").is_err());
    }

    #[test]
    fn test_validate_file_path_sensitive_files() {
        assert!(validate_file_path(".ssh/id_rsa").is_err());
        assert!(validate_file_path(".aws/credentials").is_err());
        assert!(validate_file_path(".env").is_err());
    }

    #[test]
    fn test_validate_file_path_valid() {
        assert!(validate_file_path("src/main.rs").is_ok());
        assert!(validate_file_path("Cargo.toml").is_ok());
        assert!(validate_file_path("tests/test.rs").is_ok());
    }

    #[test]
    fn test_validate_command_dangerous() {
        assert!(validate_command("rm -rf /").is_err());
        assert!(validate_command("mkfs").is_err());
    }

    #[test]
    fn test_validate_command_injection() {
        assert!(validate_command("cargo test && rm -rf /").is_err());
        assert!(validate_command("echo test | bash").is_err());
        assert!(validate_command("echo $(cat /etc/passwd)").is_err());
    }

    #[test]
    fn test_validate_command_valid() {
        assert!(validate_command("cargo test").is_ok());
        assert!(validate_command("cargo build --release").is_ok());
        assert!(validate_command("rustfmt src/main.rs").is_ok());
    }
}
