use serde::{Deserialize, Serialize};

/// Represents an action that the AI wants to perform
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentAction {
    /// Read one or more files (executor decides parallelization)
    ReadFile {
        paths: Vec<String>,
    },
    /// Write or create a file
    WriteFile {
        path: String,
        content: String,
    },
    /// Make targeted edits to a file by replacing specific text
    EditFile {
        path: String,
        old_string: String,
        new_string: String,
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
        timeout: Option<u64>,
    },
    /// Git diff for one or more paths (executor decides parallelization)
    GitDiff {
        paths: Vec<Option<String>>,
    },
    GitCommit {
        message: String,
        files: Vec<String>,
    },
    GitStatus,
    /// Web search via Ollama Cloud API (executor decides parallelization)
    WebSearch {
        queries: Vec<(String, usize)>,
    },
    /// Fetch a URL's content via Ollama Cloud API
    WebFetch {
        url: String,
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
    /// Type of action (e.g., "Write", "Bash", "Read", "Edit", "Delete", "Web Search", "Web Fetch")
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
    /// Duration of long-running actions in seconds (for display)
    pub duration_seconds: Option<f64>,
    /// Multiple targets for parallel operations (e.g., multiple files, searches)
    pub targets: Option<Vec<String>>,
    /// Number of items in parallel operation
    pub item_count: Option<usize>,
    /// Items that failed initially but were retried
    pub failed_items: Option<Vec<String>>,
}
