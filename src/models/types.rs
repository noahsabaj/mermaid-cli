use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

/// Represents the context of the current project
#[derive(Debug, Clone)]
pub struct ProjectContext {
    /// Root directory of the project
    pub root_path: String,
    /// Map of file paths to their contents
    pub files: HashMap<String, String>,
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
            files: HashMap::new(),
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
        let mut context = String::new();

        if let Some(project_type) = &self.project_type {
            context.push_str(&format!("Project type: {}\n", project_type));
        }

        context.push_str(&format!("Project root: {}\n", self.root_path));
        context.push_str(&format!("Files in context: {}\n\n", self.files.len()));

        // Add file tree structure
        context.push_str("Project structure:\n");
        for path in self.files.keys() {
            context.push_str(&format!("  - {}\n", path));
        }
        context.push('\n');

        // Add explicitly included files
        if !self.included_files.is_empty() {
            context.push_str("Relevant file contents:\n");
            for file_path in &self.included_files {
                if let Some(content) = self.files.get(file_path) {
                    context.push_str(&format!("\n=== {} ===\n", file_path));
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
                "You are Mermaid, an AI pair programmer. You help users write, debug, and improve code. \
                You can read and write files, execute commands, and provide intelligent assistance. \
                Be concise and practical in your responses.".to_string()
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