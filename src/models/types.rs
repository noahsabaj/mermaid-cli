use crate::agents::ActionDisplay;
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
    /// Base64-encoded images/PDFs for multimodal models
    #[serde(default)]
    pub images: Option<Vec<String>>,
    /// Tool calls from the model (Ollama native function calling)
    #[serde(default)]
    pub tool_calls: Option<Vec<crate::models::tool_call::ToolCall>>,
    /// Tool call ID for tool result messages (OpenAI-compatible format)
    /// This links the tool result back to the original tool_call from the assistant
    #[serde(default)]
    pub tool_call_id: Option<String>,
    /// Tool name for tool result messages (required by Ollama API)
    /// This tells the model which function's result is being returned
    #[serde(default)]
    pub tool_name: Option<String>,
}

impl ChatMessage {
    /// Create a user message
    pub fn user(content: impl Into<String>) -> Self {
        Self::new(MessageRole::User, content.into())
    }

    /// Create an assistant message
    pub fn assistant(content: impl Into<String>) -> Self {
        Self::new(MessageRole::Assistant, content.into())
    }

    /// Create a system message
    pub fn system(content: impl Into<String>) -> Self {
        Self::new(MessageRole::System, content.into())
    }

    /// Create a tool result message
    pub fn tool(
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            role: MessageRole::Tool,
            content: content.into(),
            timestamp: chrono::Local::now(),
            actions: Vec::new(),
            thinking: None,
            images: None,
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
            tool_name: Some(tool_name.into()),
        }
    }

    /// Base constructor with role and content
    fn new(role: MessageRole, content: String) -> Self {
        Self {
            role,
            content,
            timestamp: chrono::Local::now(),
            actions: Vec::new(),
            thinking: None,
            images: None,
            tool_calls: None,
            tool_call_id: None,
            tool_name: None,
        }
    }

    /// Builder: attach images
    pub fn with_images(mut self, images: Vec<String>) -> Self {
        self.images = Some(images);
        self
    }

    /// Builder: attach tool calls
    pub fn with_tool_calls(mut self, tool_calls: Vec<crate::models::tool_call::ToolCall>) -> Self {
        self.tool_calls = if tool_calls.is_empty() {
            None
        } else {
            Some(tool_calls)
        };
        self
    }

    /// Extract thinking blocks from message content
    /// Returns (thinking_content, answer_content)
    ///
    /// Safety: `str::find()` returns byte offsets. The markers "Thinking..." and
    /// "...done thinking." are pure ASCII, so adding their `.len()` to the byte
    /// offset always lands on a valid UTF-8 char boundary.
    pub fn extract_thinking(text: &str) -> (Option<String>, String) {
        // Check if the text contains thinking blocks
        if !text.contains("Thinking...") {
            return (None, text.to_string());
        }

        // Find thinking block boundaries
        if let Some(thinking_start) = text.find("Thinking...")
            && let Some(thinking_end) = text.find("...done thinking.")
        {
            // Extract thinking content (everything between markers)
            let thinking_content_start = thinking_start + "Thinking...".len();
            let thinking_text = text[thinking_content_start..thinking_end]
                .trim()
                .to_string();

            // Extract answer (everything after thinking block)
            let answer_start = thinking_end + "...done thinking.".len();
            let answer_text = text[answer_start..].trim().to_string();

            return (Some(thinking_text), answer_text);
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
    /// Tool result message (OpenAI-compatible format for function calling)
    Tool,
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
    /// Tool calls from the model (Ollama native function calling)
    pub tool_calls: Option<Vec<crate::models::tool_call::ToolCall>>,
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

    #[test]
    fn test_message_role_equality() {
        let user1 = MessageRole::User;
        let user2 = MessageRole::User;
        let assistant = MessageRole::Assistant;

        assert_eq!(user1, user2, "User roles should be equal");
        assert_ne!(user1, assistant, "Different roles should not be equal");
    }

    #[test]
    fn test_chat_message_constructors() {
        let user = ChatMessage::user("Hello!");
        assert_eq!(user.role, MessageRole::User);
        assert_eq!(user.content, "Hello!");
        assert!(user.tool_calls.is_none());

        let assistant = ChatMessage::assistant("Hi there");
        assert_eq!(assistant.role, MessageRole::Assistant);

        let system = ChatMessage::system("You are helpful");
        assert_eq!(system.role, MessageRole::System);

        let tool = ChatMessage::tool("call_1", "read_file", "file contents");
        assert_eq!(tool.role, MessageRole::Tool);
        assert_eq!(tool.tool_call_id, Some("call_1".to_string()));
        assert_eq!(tool.tool_name, Some("read_file".to_string()));
    }

    #[test]
    fn test_chat_message_builders() {
        let msg = ChatMessage::user("test").with_images(vec!["base64data".to_string()]);
        assert_eq!(msg.images, Some(vec!["base64data".to_string()]));
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
            tool_calls: None,
        };

        assert_eq!(response.content, "Hello, world!");
        assert!(response.usage.is_some());
        assert_eq!(response.model_name, "ollama/tinyllama");
        assert_eq!(response.usage.unwrap().total_tokens, 150);
        assert!(response.tool_calls.is_none());
    }
}
