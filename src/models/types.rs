use crate::agents::ActionDisplay;
use serde::{Deserialize, Serialize};

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
    /// Anthropic thinking-block signature — encrypted server state that
    /// MUST round-trip back into the next request when extended thinking
    /// is enabled, or the API returns 400 `invalid_request_error`. Set
    /// only by the Anthropic adapter; other adapters leave it `None`
    /// and other providers ignore it on the wire.
    #[serde(default)]
    pub thinking_signature: Option<String>,
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
            thinking_signature: None,
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
            thinking_signature: None,
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

    /// Builder: attach an Anthropic thinking signature. Used by the
    /// Anthropic adapter when committing assistant messages so the
    /// signature can round-trip on the next request.
    pub fn with_thinking_signature(mut self, signature: impl Into<String>) -> Self {
        self.thinking_signature = Some(signature.into());
        self
    }

    /// Extract thinking blocks from message content.
    /// Returns `(thinking_content, answer_content)`.
    ///
    /// Performs a single `find` for the start marker; the previous version
    /// scanned twice (`contains` + `find`) and called `find("Thinking...")`
    /// again inside the if-let-chain.
    ///
    /// Safety: `str::find()` returns byte offsets. The markers `"Thinking..."`
    /// and `"...done thinking."` are pure ASCII, so adding their `.len()`
    /// always lands on a valid UTF-8 char boundary.
    pub fn extract_thinking(text: &str) -> (Option<String>, String) {
        let Some(thinking_start) = text.find("Thinking...") else {
            return (None, text.to_string());
        };
        let content_start = thinking_start + "Thinking...".len();

        if let Some(thinking_end) = text.find("...done thinking.") {
            let thinking_text = text[content_start..thinking_end].trim().to_string();
            let answer_start = thinking_end + "...done thinking.".len();
            let answer_text = text[answer_start..].trim().to_string();
            return (Some(thinking_text), answer_text);
        }

        // Start marker without end marker — thinking is still in progress.
        let thinking_text = text[content_start..].trim().to_string();
        (Some(thinking_text), String::new())
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
    /// Anthropic thinking-block signature (encrypted server state). Set
    /// only by `AnthropicAdapter`; other adapters leave it `None`. The
    /// agent loop's commit step copies this onto the resulting assistant
    /// `ChatMessage::thinking_signature` so it round-trips on the next
    /// turn — without this, multi-turn Claude conversations with
    /// extended thinking 400 with `invalid_request_error`.
    pub thinking_signature: Option<String>,
}

/// Token usage statistics
#[derive(Debug, Clone)]
pub struct TokenUsage {
    pub prompt_tokens: usize,
    pub completion_tokens: usize,
    pub total_tokens: usize,
}

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

    // --- extract_thinking ---

    #[test]
    fn extract_thinking_no_marker_returns_text_unchanged() {
        let (thinking, answer) = ChatMessage::extract_thinking("just a plain answer");
        assert_eq!(thinking, None);
        assert_eq!(answer, "just a plain answer");
    }

    #[test]
    fn extract_thinking_complete_block() {
        let raw = "Thinking...\n  reasoning here\n...done thinking.\n\nFinal answer";
        let (thinking, answer) = ChatMessage::extract_thinking(raw);
        assert_eq!(thinking.as_deref(), Some("reasoning here"));
        assert_eq!(answer, "Final answer");
    }

    #[test]
    fn thinking_signature_round_trips_through_serde() {
        // Anthropic encrypted server state — must survive
        // serialize/deserialize so saved conversations resume cleanly.
        let msg = ChatMessage::assistant("Step 3 lives.")
            .with_thinking_signature("sig_abc123_encrypted_blob");
        let json = serde_json::to_string(&msg).expect("serialize");
        let back: ChatMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(
            back.thinking_signature.as_deref(),
            Some("sig_abc123_encrypted_blob")
        );
        assert_eq!(back.content, "Step 3 lives.");
    }

    #[test]
    fn thinking_signature_defaults_to_none() {
        // Backward compat: messages saved before Step 3 won't have the
        // field. Serde default kicks in — None — and deserialize
        // succeeds without errors.
        let pre_step3_json = r#"{
            "role": "Assistant",
            "content": "hello",
            "timestamp": "2026-04-16T12:00:00-04:00"
        }"#;
        let msg: ChatMessage = serde_json::from_str(pre_step3_json).expect("backward compat");
        assert!(msg.thinking_signature.is_none());
    }

    #[test]
    fn extract_thinking_in_progress_no_end_marker() {
        let raw = "Thinking...\n  partial reasoning so far";
        let (thinking, answer) = ChatMessage::extract_thinking(raw);
        assert_eq!(thinking.as_deref(), Some("partial reasoning so far"));
        assert_eq!(answer, "");
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
            thinking_signature: None,
        };

        assert_eq!(response.content, "Hello, world!");
        assert!(response.usage.is_some());
        assert_eq!(response.model_name, "ollama/tinyllama");
        assert_eq!(response.usage.unwrap().total_tokens, 150);
        assert!(response.tool_calls.is_none());
    }
}
