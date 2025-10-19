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
    /// Web search via local Searxng
    WebSearch {
        query: String,
        result_count: usize,
    },
}

/// Result of an agent action
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ActionResult {
    Success { output: String },
    Error { error: String },
}

/// Display representation of an action for UI rendering
/// Used to show action results in Claude Code style
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionDisplay {
    /// Type of action (e.g., "Write", "Bash", "Read", "GitDiff")
    pub action_type: String,
    /// Target of the action (file path, command, etc.)
    pub target: String,
    /// Result of the action
    pub result: ActionResult,
    /// Preview of the output (truncated for display)
    pub preview: Option<String>,
    /// Line count for file operations
    pub line_count: Option<usize>,
    /// Full file content (for Write actions, to show preview)
    pub file_content: Option<String>,
}
