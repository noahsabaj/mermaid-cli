use serde::{Deserialize, Serialize};

/// Represents an action that the AI wants to perform
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentAction {
    /// Read a file
    ReadFile {
        path: String,
    },
    /// Write or create a file
    WriteFile {
        path: String,
        content: String,
    },
    /// Delete a file
    DeleteFile {
        path: String,
    },
    /// Create a directory
    CreateDirectory {
        path: String,
    },
    /// Execute a shell command
    ExecuteCommand {
        command: String,
        working_dir: Option<String>,
    },
    /// Git operations
    GitDiff {
        path: Option<String>,
    },
    GitCommit {
        message: String,
        files: Vec<String>,
    },
    GitStatus,
}

/// Result of an agent action
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ActionResult {
    Success {
        output: String,
    },
    Error {
        error: String,
    },
}