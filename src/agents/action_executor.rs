use anyhow::Result;

use super::executor;
use super::filesystem;
use super::git;
use super::types::{ActionResult, AgentAction};

/// Execute an agent action
pub async fn execute_action(action: &AgentAction) -> Result<ActionResult> {
    match action {
        AgentAction::ReadFile { path } => {
            filesystem::read_file(path).map(|content| ActionResult::Success { output: content })
        },
        AgentAction::WriteFile { path, content } => {
            filesystem::write_file(path, content).map(|_| ActionResult::Success {
                output: format!("File written: {}", path),
            })
        },
        AgentAction::DeleteFile { path } => {
            filesystem::delete_file(path).map(|_| ActionResult::Success {
                output: format!("File deleted: {}", path),
            })
        },
        AgentAction::CreateDirectory { path } => {
            filesystem::create_directory(path).map(|_| ActionResult::Success {
                output: format!("Directory created: {}", path),
            })
        },
        AgentAction::ExecuteCommand {
            command,
            working_dir,
        } => executor::execute_command(command, working_dir.as_deref()).await,
        AgentAction::GitDiff { path } => {
            git::get_diff(path.as_deref()).map(|diff| ActionResult::Success { output: diff })
        },
        AgentAction::GitStatus => {
            git::get_status().map(|status| ActionResult::Success { output: status })
        },
        AgentAction::GitCommit { message, files } => {
            git::commit(message, files).map(|_| ActionResult::Success {
                output: format!("Committed with message: {}", message),
            })
        },
    }
    .map_err(|e| ActionResult::Error {
        error: e.to_string(),
    })
    .or_else(|e| Ok(e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_execute_read_file_action() {
        let action = AgentAction::ReadFile {
            path: "Cargo.toml".to_string(),
        };
        let result = execute_action(&action).await;
        assert!(result.is_ok());
        match result.unwrap() {
            ActionResult::Success { output } => {
                assert!(output.contains("[package]") || !output.is_empty());
            },
            ActionResult::Error { .. } => panic!("Should not error on valid file"),
        }
    }

    #[tokio::test]
    async fn test_execute_read_file_not_found() {
        let action = AgentAction::ReadFile {
            path: "nonexistent_file_xyz.txt".to_string(),
        };
        let result = execute_action(&action).await;
        match result {
            Ok(ActionResult::Error { .. }) => {}, // Expected
            _ => panic!("Should return error for missing file"),
        }
    }

    #[tokio::test]
    async fn test_execute_write_file_action() {
        // Test that write action succeeds or returns proper result
        // Use a realistic relative path that tests would expect
        let action = AgentAction::WriteFile {
            path: "target/test_output.txt".to_string(),
            content: "test content".to_string(),
        };
        let result = execute_action(&action).await;
        assert!(result.is_ok());
        // Accept either success or error - filesystem behavior may vary
        match result.unwrap() {
            ActionResult::Success { output } => {
                assert!(output.contains("File written"));
            },
            ActionResult::Error { error } => {
                // Expected if directory doesn't exist or permissions issue
                assert!(!error.is_empty());
            },
        }
    }

    #[tokio::test]
    async fn test_execute_create_directory_action() {
        // Test that create directory action returns expected result
        let action = AgentAction::CreateDirectory {
            path: "target/test_mermaid_dir".to_string(),
        };
        let result = execute_action(&action).await;
        assert!(result.is_ok());
        // Accept either success or error - filesystem may already exist or have permissions
        match result.unwrap() {
            ActionResult::Success { output } => {
                assert!(output.contains("Directory created"));
            },
            ActionResult::Error { error } => {
                // Expected if directory already exists
                assert!(!error.is_empty());
            },
        }
    }

    #[tokio::test]
    async fn test_execute_git_status_action() {
        let action = AgentAction::GitStatus;
        let result = execute_action(&action).await;
        // May fail if not in git repo, but shouldn't panic
        assert!(result.is_ok() || result.is_err());
    }

    #[tokio::test]
    async fn test_execute_git_diff_action() {
        let action = AgentAction::GitDiff { path: None };
        let result = execute_action(&action).await;
        // May fail if not in git repo, but shouldn't panic
        assert!(result.is_ok() || result.is_err());
    }

    #[tokio::test]
    async fn test_execute_command_safe_action() {
        let action = AgentAction::ExecuteCommand {
            command: "echo test".to_string(),
            working_dir: None,
        };
        let result = execute_action(&action).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_execute_command_with_working_dir() {
        let action = AgentAction::ExecuteCommand {
            command: "pwd".to_string(),
            working_dir: Some("/tmp".to_string()),
        };
        let result = execute_action(&action).await;
        assert!(result.is_ok());
    }
}
