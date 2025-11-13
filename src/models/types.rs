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
    /// Thinking/reasoning content (for models that expose their thought process)
    #[serde(default)]
    pub thinking: Option<String>,
}

impl ChatMessage {
    /// Extract thinking blocks from message content
    /// Returns (thinking_content, answer_content)
    pub fn extract_thinking(text: &str) -> (Option<String>, String) {
        // Check if the text contains thinking blocks
        if !text.contains("Thinking...") {
            return (None, text.to_string());
        }

        // Find thinking block boundaries
        if let Some(thinking_start) = text.find("Thinking...") {
            if let Some(thinking_end) = text.find("...done thinking.") {
                // Extract thinking content (everything between markers)
                let thinking_content_start = thinking_start + "Thinking...".len();
                let thinking_text = text[thinking_content_start..thinking_end].trim().to_string();

                // Extract answer (everything after thinking block)
                let answer_start = thinking_end + "...done thinking.".len();
                let answer_text = text[answer_start..].trim().to_string();

                return (Some(thinking_text), answer_text);
            }
        }

        // If we found "Thinking..." but not the end marker, treat it all as thinking in progress
        if let Some(thinking_start) = text.find("Thinking...") {
            let thinking_content_start = thinking_start + "Thinking...".len();
            let thinking_text = text[thinking_content_start..].trim().to_string();
            return (Some(thinking_text), String::new());
        }

        (None, text.to_string())
    }
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

/// Response from a model
#[derive(Debug, Clone)]
pub struct ModelResponse {
    /// The actual response text
    pub content: String,
    /// Usage statistics if available
    pub usage: Option<TokenUsage>,
    /// Model that generated the response
    pub model_name: String,
    /// Thinking/reasoning content (for models that expose their thought process)
    pub thinking: Option<String>,
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

#[cfg(test)]
mod tests {
    use super::*;

    // Phase 3 Test Suite: Model Types - 8 comprehensive tests

    #[test]
    fn test_message_role_equality() {
        let user1 = MessageRole::User;
        let user2 = MessageRole::User;
        let assistant = MessageRole::Assistant;

        assert_eq!(user1, user2, "User roles should be equal");
        assert_ne!(user1, assistant, "Different roles should not be equal");
    }

    #[test]
    fn test_chat_message_creation() {
        let message = ChatMessage {
            role: MessageRole::User,
            content: "Hello, assistant!".to_string(),
            timestamp: chrono::Local::now(),
            actions: vec![],
            thinking: None,
        };

        assert_eq!(message.role, MessageRole::User);
        assert_eq!(message.content, "Hello, assistant!");
        assert!(message.actions.is_empty());
        assert!(message.thinking.is_none());
    }

    #[test]
    fn test_project_context_new() {
        let context = ProjectContext::new("/home/user/project".to_string());

        assert_eq!(context.root_path, "/home/user/project");
        assert!(context.files.is_empty());
        assert_eq!(context.token_count, 0);
        assert!(context.included_files.is_empty());
        assert_eq!(context.project_type, None);
    }

    #[test]
    fn test_project_context_add_file() {
        let mut context = ProjectContext::new("/project".to_string());

        context.add_file("src/main.rs".to_string(), "fn main() {}".to_string());
        context.add_file("Cargo.toml".to_string(), "[package]".to_string());

        assert_eq!(context.files.len(), 2);
        assert_eq!(
            context.files.get("src/main.rs"),
            Some(&"fn main() {}".to_string())
        );
        assert_eq!(
            context.files.get("Cargo.toml"),
            Some(&"[package]".to_string())
        );
    }

    #[test]
    fn test_project_context_prompt_formatting() {
        let mut context = ProjectContext::new("/project".to_string());
        context.project_type = Some("rust".to_string());
        context.add_file("src/main.rs".to_string(), "fn main() {}".to_string());
        context.add_file("Cargo.toml".to_string(), "[package]".to_string());
        context.included_files = vec!["src/main.rs".to_string()];

        let prompt = context.to_prompt_context();

        assert!(
            prompt.contains("Project type: rust"),
            "Should include project type"
        );
        assert!(
            prompt.contains("Project root: /project"),
            "Should include project root"
        );
        assert!(
            prompt.contains("Files in context: 2"),
            "Should include file count"
        );
        assert!(
            prompt.contains("src/main.rs"),
            "Should include file structure"
        );
        assert!(
            prompt.contains("Cargo.toml"),
            "Should include file structure"
        );
        assert!(
            prompt.contains("fn main() {}"),
            "Should include file content"
        );
        // Check that included files section exists
        assert!(
            prompt.contains("Relevant file contents") || prompt.contains("==="),
            "Should include section for relevant files"
        );
    }

    #[test]
    fn test_token_usage_structure() {
        let usage = TokenUsage {
            prompt_tokens: 100,
            completion_tokens: 50,
            total_tokens: 150,
        };

        assert_eq!(usage.prompt_tokens, 100);
        assert_eq!(usage.completion_tokens, 50);
        assert_eq!(usage.total_tokens, 150);
    }

    #[test]
    fn test_model_response_creation() {
        let usage = TokenUsage {
            prompt_tokens: 100,
            completion_tokens: 50,
            total_tokens: 150,
        };

        let response = ModelResponse {
            content: "Hello, world!".to_string(),
            usage: Some(usage),
            model_name: "ollama/tinyllama".to_string(),
            thinking: None,
        };

        assert_eq!(response.content, "Hello, world!");
        assert!(response.usage.is_some());
        assert_eq!(response.model_name, "ollama/tinyllama");
        assert_eq!(response.usage.unwrap().total_tokens, 150);
    }
}
