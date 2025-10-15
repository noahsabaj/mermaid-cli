use crate::agents::ActionDisplay;
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Represents a chat message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: MessageRole,
    pub content: String,
    pub timestamp: chrono::DateTime<chrono::Local>,
    /// Actions performed during this message (for display purposes)
    #[serde(default)]
    pub actions: Vec<ActionDisplay>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MessageRole {
    User,
    Assistant,
    System,
}

/// Represents the context of the current project
#[derive(Debug, Clone)]
pub struct ProjectContext {
    /// Root directory of the project
    pub root_path: String,
    /// Map of file paths to their contents (using FxHashMap for performance)
    pub files: FxHashMap<String, String>,
    /// Project type (e.g., "rust", "python", "javascript")
    pub project_type: Option<String>,
    /// Total token count of the context
    pub token_count: usize,
    /// Files to explicitly include in context
    pub included_files: Vec<String>,
}

impl ProjectContext {
    pub fn new(root_path: String) -> Self {
        Self {
            root_path,
            files: FxHashMap::default(),
            project_type: None,
            token_count: 0,
            included_files: Vec::new(),
        }
    }

    /// Add a file to the context
    pub fn add_file(&mut self, path: String, content: String) {
        self.files.insert(path, content);
    }

    /// Get a formatted string of the project context for the model
    pub fn to_prompt_context(&self) -> String {
        // Pre-calculate capacity to reduce allocations
        let header_size = 100; // Approximate size of headers
        let file_list_size = self.files.keys().map(|k| k.len() + 5).sum::<usize>(); // "  - path\n"
        let content_size: usize = self.included_files.iter()
            .filter_map(|path| self.files.get(path).map(|content| (path, content)))
            .map(|(path, content)| content.len() + path.len() + 20) // path + decorators
            .sum();

        let capacity = header_size + file_list_size + content_size;
        let mut context = String::with_capacity(capacity);

        if let Some(project_type) = &self.project_type {
            context.push_str("Project type: ");
            context.push_str(project_type);
            context.push('\n');
        }

        context.push_str("Project root: ");
        context.push_str(&self.root_path);
        context.push_str("\nFiles in context: ");
        context.push_str(&self.files.len().to_string());
        context.push_str("\n\n");

        // Add file tree structure
        context.push_str("Project structure:\n");
        for path in self.files.keys() {
            context.push_str("  - ");
            context.push_str(path);
            context.push('\n');
        }
        context.push('\n');

        // Add explicitly included files
        if !self.included_files.is_empty() {
            context.push_str("Relevant file contents:\n");
            for file_path in &self.included_files {
                if let Some(content) = self.files.get(file_path) {
                    context.push_str("\n=== ");
                    context.push_str(file_path);
                    context.push_str(" ===\n");
                    context.push_str(content);
                    context.push_str("\n=== end ===\n");
                }
            }
        }

        context
    }
}

/// Configuration for model parameters
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    pub temperature: Option<f32>,
    pub max_tokens: Option<usize>,
    pub top_p: Option<f32>,
    pub frequency_penalty: Option<f32>,
    pub presence_penalty: Option<f32>,
    pub system_prompt: Option<String>,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            temperature: Some(0.7),
            max_tokens: Some(4096),
            top_p: Some(1.0),
            frequency_penalty: None,
            presence_penalty: None,
            system_prompt: Some(
                r#"You are Mermaid, an AI pair programmer assistant that can read, write, and execute code.

## IMPORTANT: Action Blocks

You have the ability to perform actions by using special action blocks in your responses. Action blocks are automatically parsed and executed, and the results are displayed cleanly to the user.

**Key point**: Action blocks themselves are NOT shown to the user. Instead, the system displays clean action summaries with results. This means you should write natural, explanatory text AROUND action blocks to provide context.

### File Operations

To write or create a file, use:
```
[FILE_WRITE: path/to/file.rs]
fn main() {
    println!("Hello, world!");
}
[/FILE_WRITE]
```

To read a file, use:
```
[FILE_READ: path/to/file.rs]
[/FILE_READ]
```

### Shell Commands

To execute shell commands, use:
```
[COMMAND: cargo test]
[/COMMAND]
```

Or with a specific working directory:
```
[COMMAND: cargo build --release dir=/path/to/project]
[/COMMAND]
```

### Git Operations

To see git status:
```
[GIT_STATUS]
```

To see git diff:
```
[GIT_DIFF]
```

## Guidelines

1. When asked to create or modify files, ALWAYS use the [FILE_WRITE:] action block
2. When asked to run commands, ALWAYS use the [COMMAND:] action block
3. Be precise with file paths - use the exact paths shown in the project context
4. After writing files, consider running relevant tests or build commands
5. Provide natural explanatory text BEFORE and AFTER action blocks, since the blocks themselves won't be shown
6. Don't repeat code content in your explanation - the action summary will show what was done

Example good response:
"I'll create a new configuration file for you.

[FILE_WRITE: config.toml]
[database]
host = "localhost"
[/FILE_WRITE]

The configuration file has been created with the default settings. You can modify the host value if needed."

Remember: You're not just showing code examples - you can actually create, modify, and execute files! The user sees your explanations and clean action summaries, not the raw action blocks."#.to_string()
            ),
        }
    }
}

/// Response from a model
#[derive(Debug, Clone)]
pub struct ModelResponse {
    /// The actual response text
    pub content: String,
    /// Usage statistics if available
    pub usage: Option<TokenUsage>,
    /// Model that generated the response
    pub model_name: String,
}

/// Token usage statistics
#[derive(Debug, Clone)]
pub struct TokenUsage {
    pub prompt_tokens: usize,
    pub completion_tokens: usize,
    pub total_tokens: usize,
}

/// Stream callback type for real-time response streaming
pub type StreamCallback = Arc<dyn Fn(&str) + Send + Sync>;

/// Capabilities of a model
#[derive(Debug, Clone)]
pub struct ModelCapabilities {
    pub max_context_length: usize,
    pub supports_streaming: bool,
    pub supports_functions: bool,
    pub supports_vision: bool,
}

impl Default for ModelCapabilities {
    fn default() -> Self {
        Self {
            max_context_length: 4096,
            supports_streaming: true,
            supports_functions: false,
            supports_vision: false,
        }
    }
}
