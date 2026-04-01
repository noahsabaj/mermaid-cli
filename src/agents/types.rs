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
    /// Web search via Ollama Cloud API (executor decides parallelization)
    WebSearch {
        queries: Vec<(String, usize)>,
    },
    /// Fetch a URL's content via Ollama Cloud API
    WebFetch {
        url: String,
    },
    /// Spawn an autonomous sub-agent with its own conversation context
    SpawnAgent {
        prompt: String,
        description: String,
    },
    /// Capture a screenshot of the screen (or a focused window/monitor/region/window)
    Screenshot {
        mode: String,            // "fullscreen", "focused", "monitor", "region", "window"
        monitor: Option<String>, // monitor name for "monitor" mode (e.g., "DP-0")
        region: Option<String>,  // "X,Y,WIDTHxHEIGHT" for "region" mode
        window: Option<String>,  // window title for "window" mode (e.g., "Discord")
    },
    /// Click at screen coordinates
    Click { x: i32, y: i32, button: String },
    /// Type a text string at the current cursor position
    TypeText { text: String },
    /// Press a key or key combination
    PressKey { key: String },
    /// Scroll in a direction
    Scroll { direction: String, amount: i32 },
    /// Move mouse cursor to coordinates
    MouseMove { x: i32, y: i32 },
    /// List all visible window titles (lightweight, no screenshot)
    ListWindows,
    /// Dynamic MCP tool call (dispatched to an MCP server at runtime)
    McpToolCall {
        server_name: String,
        tool_name: String,
        arguments: serde_json::Value,
    },
    /// Placeholder for tool calls that failed to parse (never executed)
    ParseError {
        message: String,
    },
}

/// Result of an agent action
#[derive(Debug, Clone, Serialize, Deserialize)]
#[must_use]
pub enum ActionResult {
    Success {
        output: String,
        #[serde(default)]
        images: Option<Vec<String>>,
    },
    Error {
        error: String,
    },
}

impl AgentAction {
    /// Extract a (type_label, target) pair for display or logging
    pub fn display_info(&self) -> (&str, String) {
        match self {
            AgentAction::ReadFile { paths } => {
                if paths.len() == 1 {
                    ("Read", paths[0].clone())
                } else {
                    ("Read", format!("{} files", paths.len()))
                }
            },
            AgentAction::WriteFile { path, .. } => ("Write", path.clone()),
            AgentAction::EditFile { path, .. } => ("Edit", path.clone()),
            AgentAction::DeleteFile { path } => ("Delete", path.clone()),
            AgentAction::CreateDirectory { path } => ("Bash", format!("mkdir -p {}", path)),
            AgentAction::ExecuteCommand { command, .. } => ("Bash", command.clone()),
            AgentAction::WebSearch { queries } => {
                if queries.len() == 1 {
                    ("Web Search", queries[0].0.clone())
                } else {
                    ("Web Search", format!("{} queries", queries.len()))
                }
            },
            AgentAction::WebFetch { url } => ("Web Fetch", url.clone()),
            AgentAction::SpawnAgent { description, .. } => ("Agent", description.clone()),
            AgentAction::Screenshot { mode, window, .. } => {
                let target = match mode.as_str() {
                    "focused" => "focused window".to_string(),
                    "monitor" => "monitor".to_string(),
                    "region" => "region".to_string(),
                    "window" => {
                        format!("window \"{}\"", window.as_deref().unwrap_or("?"))
                    },
                    _ => "screen capture".to_string(),
                };
                ("Screenshot", target)
            },
            AgentAction::Click { x, y, button } => {
                ("Click", format!("({}, {}) {}", x, y, button))
            },
            AgentAction::TypeText { text } => {
                ("Type", text.chars().take(30).collect())
            },
            AgentAction::PressKey { key } => ("Key", key.clone()),
            AgentAction::Scroll { direction, amount } => {
                ("Scroll", format!("{} {}", direction, amount))
            },
            AgentAction::MouseMove { x, y } => ("Move", format!("({}, {})", x, y)),
            AgentAction::ListWindows => ("ListWindows", "visible windows".to_string()),
            AgentAction::McpToolCall {
                server_name,
                tool_name,
                ..
            } => ("MCP", format!("{}:{}", server_name, tool_name)),
            AgentAction::ParseError { message } => ("Error", message.clone()),
        }
    }
}

/// Display representation of an action for UI rendering
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionDisplay {
    /// Type of action (e.g., "Write", "Bash", "Read", "Edit", "Delete", "Agent")
    pub action_type: String,
    /// Target of the action (file path, command, etc.)
    pub target: String,
    /// Result of the action
    pub result: ActionResult,
    /// Type-specific display data
    #[serde(default)]
    pub details: ActionDetails,
    /// Duration of long-running actions in seconds
    pub duration_seconds: Option<f64>,
}

/// Type-specific display data for action results
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub enum ActionDetails {
    /// No extra display data (Delete, CreateDirectory, or old conversations)
    #[default]
    Simple,
    /// Text preview with optional line count (Read, Bash, Git, WebSearch, etc.)
    Preview {
        text: String,
        line_count: Option<usize>,
    },
    /// File write with content for syntax-highlighted preview
    FileContent { line_count: usize, content: String },
    /// File edit with summary and diff for color-coded display
    Diff { summary: String, diff: String },
    /// Agent completion with summary and tool use count
    Agent { summary: String, tool_uses: usize },
}

