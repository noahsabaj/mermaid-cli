use crate::domain::ActionDisplay;
use serde::{Deserialize, Serialize};

/// Represents a chat message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: MessageRole,
    pub content: String,
    pub timestamp: chrono::DateTime<chrono::Local>,
    /// Mermaid-owned message classification. Provider adapters ignore
    /// this; render/persistence use it to distinguish generated
    /// checkpoints from normal user/assistant turns.
    #[serde(default)]
    pub kind: ChatMessageKind,
    /// Optional Mermaid-owned structured metadata for UI/replay.
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
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

    /// Create a display-only run summary (e.g. "Worked for 5m 12s · used 12.3k
    /// tokens"). Rendered dim/italic where the spinner was; excluded from the
    /// model context by `build_chat_request` so it never bloats the conversation.
    pub fn run_summary(content: impl Into<String>) -> Self {
        let mut m = Self::new(MessageRole::System, content.into());
        m.kind = ChatMessageKind::RunSummary;
        m
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
            kind: ChatMessageKind::Normal,
            metadata: None,
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
            kind: ChatMessageKind::Normal,
            metadata: None,
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

#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum MessageRole {
    User,
    Assistant,
    System,
    /// Tool result message (OpenAI-compatible format for function calling)
    Tool,
}

// F74: version-skew-tolerant deserialize. A conversation written by a NEWER build
// may carry a `role` string this build doesn't model; the derived `Deserialize`
// would hard-fail the WHOLE `ConversationHistory` parse, so `--continue` silently
// skipped the newest session. We instead accept any string and map an unknown
// role to the neutral `System` so the session still loads and resumes.
//
// We deliberately do NOT add a dedicated `MessageRole::Unknown` variant here:
// `MessageRole` is matched exhaustively by the provider adapters
// (anthropic/openai_compat/ollama/gemini) and the chat renderer/compaction
// formatter, all outside this change's allowed file set, and a new variant would
// fail those `match` arms to compile. Mapping unknown→System keeps the wire
// format and every existing `match` intact while making the parse tolerant
// ("treat as a neutral role; don't panic").
impl<'de> Deserialize<'de> for MessageRole {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Ok(match raw.as_str() {
            "User" => MessageRole::User,
            "Assistant" => MessageRole::Assistant,
            "System" => MessageRole::System,
            "Tool" => MessageRole::Tool,
            other => {
                tracing::warn!(
                    role = %other,
                    "unknown message role in saved conversation; treating as System (version skew?)"
                );
                MessageRole::System
            },
        })
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatMessageKind {
    #[default]
    Normal,
    ContextCheckpoint,
    /// A display-only run summary ("Worked for … · used … tokens"). Rendered
    /// dim/italic; excluded from the model context by `build_chat_request`.
    RunSummary,
    /// F74: a kind written by a NEWER build that this one doesn't model. Mapped
    /// here by `#[serde(other)]` instead of failing the whole conversation parse;
    /// it's neither a checkpoint nor a run summary, so every `matches!` site
    /// treats it like a normal message. (`ChatMessageKind` is never matched
    /// exhaustively, so adding this variant is compile-safe.)
    #[serde(other)]
    Unknown,
}

/// Why a model stopped generating, normalized across providers.
///
/// Providers report this as Anthropic `stop_reason`, OpenAI `finish_reason`,
/// or Gemini `finishReason`. It used to be parsed and discarded, so a
/// `max_tokens` truncation or a `content_filter`/safety block looked identical
/// to a clean finish. The agent loop now inspects it: `Length` surfaces a
/// truncation notice, and an empty `ContentFilter` finish becomes an error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    /// Normal end of turn (`end_turn`, `stop`, `stop_sequence`, `STOP`).
    Stop,
    /// Stopped to call a tool (`tool_use`, `tool_calls`).
    ToolUse,
    /// Hit the output token limit — the response is truncated.
    Length,
    /// Blocked by a content filter / safety system / recitation check.
    ContentFilter,
    /// A reason we don't specifically model; carries the raw provider string.
    Other(String),
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
    /// Why generation stopped, when the provider reported it. `None` if the
    /// provider didn't say (or the adapter doesn't yet map it).
    pub stop_reason: Option<FinishReason>,
    /// Anthropic thinking-block signature (encrypted server state). Set
    /// only by `AnthropicAdapter`; other adapters leave it `None`. The
    /// agent loop's commit step copies this onto the resulting assistant
    /// `ChatMessage::thinking_signature` so it round-trips on the next
    /// turn — without this, multi-turn Claude conversations with
    /// extended thinking 400 with `invalid_request_error`.
    pub thinking_signature: Option<String>,
}

/// Where a token count came from. Provider-reported counts are the
/// billing/request truth; estimates are only for preflight context
/// diagnostics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenUsageSource {
    #[default]
    Provider,
    Estimate,
}

/// Token usage statistics normalized across providers.
///
/// `prompt_tokens`, `completion_tokens`, and `total_tokens` preserve
/// the old public surface. Extra fields keep cache/reasoning detail so
/// UI can stop flattening unlike provider concepts into one number.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt_tokens: usize,
    pub completion_tokens: usize,
    pub total_tokens: usize,
    #[serde(default)]
    pub cached_input_tokens: usize,
    #[serde(default)]
    pub cache_creation_input_tokens: usize,
    #[serde(default)]
    pub reasoning_output_tokens: usize,
    #[serde(default)]
    pub source: TokenUsageSource,
}

impl TokenUsage {
    pub fn provider(prompt_tokens: usize, completion_tokens: usize, total_tokens: usize) -> Self {
        Self {
            prompt_tokens,
            completion_tokens,
            total_tokens,
            cached_input_tokens: 0,
            cache_creation_input_tokens: 0,
            reasoning_output_tokens: 0,
            source: TokenUsageSource::Provider,
        }
    }

    pub fn estimate(prompt_tokens: usize) -> Self {
        Self {
            prompt_tokens,
            completion_tokens: 0,
            total_tokens: prompt_tokens,
            cached_input_tokens: 0,
            cache_creation_input_tokens: 0,
            reasoning_output_tokens: 0,
            source: TokenUsageSource::Estimate,
        }
    }

    pub fn with_cached_input(mut self, cached_input_tokens: usize) -> Self {
        self.cached_input_tokens = cached_input_tokens;
        self
    }

    pub fn with_cache_creation(mut self, cache_creation_input_tokens: usize) -> Self {
        self.cache_creation_input_tokens = cache_creation_input_tokens;
        self
    }

    pub fn with_reasoning_output(mut self, reasoning_output_tokens: usize) -> Self {
        self.reasoning_output_tokens = reasoning_output_tokens;
        self
    }

    pub fn input_total_tokens(&self) -> usize {
        self.prompt_tokens
            .saturating_add(self.cached_input_tokens)
            .saturating_add(self.cache_creation_input_tokens)
    }

    pub fn output_total_tokens(&self) -> usize {
        self.completion_tokens
            .saturating_add(self.reasoning_output_tokens)
    }
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
        let usage = TokenUsage::provider(100, 50, 150)
            .with_cached_input(25)
            .with_reasoning_output(10);

        assert_eq!(usage.prompt_tokens, 100);
        assert_eq!(usage.completion_tokens, 50);
        assert_eq!(usage.total_tokens, 150);
        assert_eq!(usage.cached_input_tokens, 25);
        assert_eq!(usage.reasoning_output_tokens, 10);
        assert_eq!(usage.source, TokenUsageSource::Provider);
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
    fn unknown_message_role_deserializes_to_system() {
        // F74: a role string from a newer build must not fail the parse — it maps
        // to the neutral System role so the conversation still loads (`--continue`
        // no longer skips the newest session).
        let role: MessageRole = serde_json::from_str("\"Developer\"").expect("tolerant");
        assert_eq!(role, MessageRole::System);
        // Known roles still map correctly.
        assert_eq!(
            serde_json::from_str::<MessageRole>("\"Tool\"").unwrap(),
            MessageRole::Tool
        );
    }

    #[test]
    fn unknown_message_kind_deserializes_to_unknown() {
        // F74: an unknown ChatMessageKind maps to Unknown via #[serde(other)]
        // rather than failing the parse; it's treated like a normal message.
        let kind: ChatMessageKind =
            serde_json::from_str("\"some_future_kind\"").expect("tolerant");
        assert_eq!(kind, ChatMessageKind::Unknown);
        assert_ne!(kind, ChatMessageKind::Normal);
    }

    #[test]
    fn chat_message_with_unknown_role_round_trips() {
        // The whole ChatMessage deserialize succeeds despite an unknown role.
        let json = r#"{
            "role": "Developer",
            "content": "hi",
            "timestamp": "2026-04-16T12:00:00-04:00"
        }"#;
        let msg: ChatMessage = serde_json::from_str(json).expect("tolerant");
        assert_eq!(msg.role, MessageRole::System);
        assert_eq!(msg.content, "hi");
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
        let usage = TokenUsage::provider(100, 50, 150);

        let response = ModelResponse {
            content: "Hello, world!".to_string(),
            usage: Some(usage),
            model_name: "ollama/tinyllama".to_string(),
            thinking: None,
            tool_calls: None,
            stop_reason: None,
            thinking_signature: None,
        };

        assert_eq!(response.content, "Hello, world!");
        assert!(response.usage.is_some());
        assert_eq!(response.model_name, "ollama/tinyllama");
        assert_eq!(response.usage.unwrap().total_tokens, 150);
        assert!(response.tool_calls.is_none());
    }
}
