use crate::action::ActionDisplay;
use serde::{Deserialize, Serialize};

/// Opaque provider-owned state that must be replayed with a committed assistant
/// turn. The reducer carries this as inert data; only the matching provider
/// interprets it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "snake_case")]
pub enum ProviderContinuation {
    /// Anthropic's signed extended-thinking block.
    Anthropic { signature: String },
    /// Meta Responses output items, including encrypted reasoning state.
    MetaResponses { output: Vec<MetaResponseItem> },
}

impl ProviderContinuation {
    pub fn anthropic_signature(&self) -> Option<&str> {
        match self {
            Self::Anthropic { signature } => Some(signature),
            Self::MetaResponses { .. } => None,
        }
    }

    pub fn meta_output(&self) -> Option<&[MetaResponseItem]> {
        match self {
            Self::MetaResponses { output } => Some(output),
            Self::Anthropic { .. } => None,
        }
    }

    pub fn retain_meta_function_calls(&mut self, mut keep: impl FnMut(&str) -> bool) {
        if let Self::MetaResponses { output } = self {
            output.retain(|item| item.function_call_id().is_none_or(&mut keep));
        }
    }
}

/// One Meta Responses output item saved for stateless replay. Reasoning items
/// split their encrypted payload from the remaining JSON so the ciphertext can
/// be serialized as base64 bytes. This keeps generic persistence redaction from
/// mistaking a ciphertext for a credential and corrupting the replay state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MetaResponseItem {
    Reasoning {
        item: serde_json::Value,
        #[serde(with = "crate::utils::serde_base64::string")]
        encrypted_content: String,
    },
    Other {
        item: serde_json::Value,
    },
}

impl MetaResponseItem {
    pub fn from_wire(mut item: serde_json::Value) -> Self {
        let is_reasoning =
            item.get("type").and_then(serde_json::Value::as_str) == Some("reasoning");
        if is_reasoning
            && let Some(encrypted) = item
                .as_object_mut()
                .and_then(|object| object.remove("encrypted_content"))
                .and_then(|value| value.as_str().map(str::to_string))
        {
            return Self::Reasoning {
                item,
                encrypted_content: encrypted,
            };
        }
        Self::Other { item }
    }

    pub fn to_wire(&self) -> serde_json::Value {
        match self {
            Self::Reasoning {
                item,
                encrypted_content,
            } => {
                let mut item = item.clone();
                if let Some(object) = item.as_object_mut() {
                    object.insert(
                        "encrypted_content".to_string(),
                        serde_json::Value::String(encrypted_content.clone()),
                    );
                }
                item
            },
            Self::Other { item } => item.clone(),
        }
    }

    pub fn function_call_id(&self) -> Option<&str> {
        let item = match self {
            Self::Reasoning { item, .. } | Self::Other { item } => item,
        };
        (item.get("type").and_then(serde_json::Value::as_str) == Some("function_call"))
            .then(|| item.get("call_id").and_then(serde_json::Value::as_str))
            .flatten()
    }
}

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
    /// Global `[Image #N]` display numbers, parallel to `images` (same length,
    /// same order). Kept separate from `images` so provider adapters and the
    /// `/context` image count keep reading `images: Vec<String>` unchanged, and
    /// so sessions saved before image numbering deserialize cleanly (`None` →
    /// the transcript falls back to a positional index).
    #[serde(default)]
    pub image_numbers: Option<Vec<u64>>,
    /// Tool calls from the model (Ollama native function calling)
    #[serde(default)]
    pub tool_calls: Option<Vec<crate::models::tool_call::ToolCall>>,
    /// Tool call ID for tool result messages (OpenAI-compatible format)
    /// This links the tool result back to the original `tool_call` from the assistant
    #[serde(default)]
    pub tool_call_id: Option<String>,
    /// Tool name for tool result messages (required by Ollama API)
    /// This tells the model which function's result is being returned
    #[serde(default)]
    pub tool_name: Option<String>,
    /// Provider-owned continuation state. Anthropic stores its signed thinking
    /// block; Meta stores ordered Responses output items for encrypted replay.
    /// Other providers leave this unset and ignore it on the wire.
    #[serde(default)]
    pub provider_continuation: Option<ProviderContinuation>,
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
            image_numbers: None,
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
            tool_name: Some(tool_name.into()),
            provider_continuation: None,
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
            image_numbers: None,
            tool_calls: None,
            tool_call_id: None,
            tool_name: None,
            provider_continuation: None,
        }
    }

    /// Builder: attach images
    pub fn with_images(mut self, images: Vec<String>) -> Self {
        self.images = Some(images);
        self
    }

    /// Builder: attach the parallel global image numbers (same length/order as
    /// `with_images`). Set together at submit time so the transcript can show
    /// each image's stable `[Image #N]`.
    pub fn with_image_numbers(mut self, numbers: Vec<u64>) -> Self {
        self.image_numbers = Some(numbers);
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

    /// Builder: attach opaque provider continuation data.
    pub fn with_provider_continuation(mut self, continuation: ProviderContinuation) -> Self {
        self.provider_continuation = Some(continuation);
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
    /// An assistant message that resumes a reply cut by the provider's
    /// per-response output cap (auto-continue). Canonical history keeps it as
    /// its own message — true to the wire, signature-safe — but the transcript
    /// stitches it into the previous assistant bubble.
    Continuation,
    /// A system nudge injected to steer exactly one request (auto-continue
    /// resume, stalled-turn retry, safety-mode-loosened note). Hidden from the
    /// transcript and swept from history at the next turn-end — it must never
    /// outlive the request it steers.
    RecoveryNudge,
    /// A persistent mode-change marker ("Plan mode is now ON …") injected by
    /// the dispatch-time context-delta injector. Sent to the model on every
    /// request — the durable timeline record that keeps history consistent
    /// with the mode — hidden from the transcript (the status band is the
    /// human announcement), and unlike `RecoveryNudge` NEVER swept.
    ContextMarker,
    /// F74: a kind written by a NEWER build that this one doesn't model. Mapped
    /// here by `#[serde(other)]` instead of failing the whole conversation parse;
    /// it's neither a checkpoint nor a run summary, so every `matches!` site
    /// treats it like a normal message. (`ChatMessageKind` is never matched
    /// exhaustively, so adding this variant is compile-safe.)
    #[serde(other)]
    Unknown,
}

/// Who a `MessageRole::System` history message is FOR.
///
/// The distinction used to be implicit, and two adapters guessed wrong: every
/// provider with an OpenAI-shaped API passed system-role history through
/// inline, while Anthropic and Gemini — whose APIs have exactly one top-level
/// system field — dropped it as "a TUI affordance, not model input". That
/// silently deleted every harness steering message on those two providers: the
/// plan-mode reminder and context markers, but also the pre-existing
/// auto-continue resume and stalled-turn nudges.
///
/// Making it explicit means an adapter can no longer guess, and
/// [`ChatMessageKind::audience`] is the single place the decision lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageAudience {
    /// Ordinary conversation content. Adapters keep their existing handling.
    Conversation,
    /// Harness steering the model MUST see even though the transcript hides
    /// it. An adapter that cannot express a mid-conversation system message
    /// has to deliver it some other way — never drop it.
    ModelDirected,
}

impl ChatMessageKind {
    /// The audience for a system-role message of this kind.
    ///
    /// Deliberately an exhaustive match with no wildcard: adding a
    /// `ChatMessageKind` fails to compile here until its audience is decided,
    /// which is the guard against another kind being silently dropped on the
    /// providers that can't carry system-role history.
    pub fn audience(self) -> MessageAudience {
        match self {
            // Injected to steer the model — the whole point is that it reads
            // them. Hidden from the transcript, never from the model.
            ChatMessageKind::RecoveryNudge | ChatMessageKind::ContextMarker => {
                MessageAudience::ModelDirected
            },
            ChatMessageKind::Normal
            | ChatMessageKind::ContextCheckpoint
            | ChatMessageKind::RunSummary
            | ChatMessageKind::Continuation
            | ChatMessageKind::Unknown => MessageAudience::Conversation,
        }
    }
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
    /// Opaque provider continuation state, when the adapter emits one.
    pub provider_continuation: Option<ProviderContinuation>,
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
/// Component fields are disjoint; totals are derived, never stored:
/// - `prompt_tokens`: fresh (non-cached) input only. Adapters subtract
///   cache reads for providers whose wire prompt count includes them.
/// - `cached_input_tokens` / `cache_creation_input_tokens`: cache read
///   and write.
/// - `completion_tokens`: non-reasoning output only. Adapters subtract
///   reasoning for providers whose wire completion count includes it.
///   Anthropic reports no separate thinking count, so its thinking
///   tokens ride inside `completion_tokens` and
///   `reasoning_output_tokens` stays 0.
/// - `reasoning_output_tokens`: disjoint from `completion_tokens`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt_tokens: usize,
    pub completion_tokens: usize,
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
    pub fn provider(prompt_tokens: usize, completion_tokens: usize) -> Self {
        Self {
            prompt_tokens,
            completion_tokens,
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

    /// Full request total, derived. Equals every provider's wire total
    /// (verified for Anthropic, OpenAI, Gemini, Ollama) — a stored total
    /// would only reintroduce per-provider drift.
    pub fn total_tokens(&self) -> usize {
        self.input_total_tokens()
            .saturating_add(self.output_total_tokens())
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
        let usage = TokenUsage::provider(100, 50)
            .with_cached_input(25)
            .with_cache_creation(5)
            .with_reasoning_output(10);

        assert_eq!(usage.prompt_tokens, 100);
        assert_eq!(usage.completion_tokens, 50);
        assert_eq!(usage.cached_input_tokens, 25);
        assert_eq!(usage.cache_creation_input_tokens, 5);
        assert_eq!(usage.reasoning_output_tokens, 10);
        assert_eq!(usage.input_total_tokens(), 130);
        assert_eq!(usage.output_total_tokens(), 60);
        assert_eq!(usage.total_tokens(), 190);
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
    fn provider_continuation_round_trips_through_serde() {
        // Anthropic encrypted server state — must survive
        // serialize/deserialize so saved conversations resume cleanly.
        let msg = ChatMessage::assistant("Step 3 lives.").with_provider_continuation(
            ProviderContinuation::Anthropic {
                signature: "sig_abc123_encrypted_blob".to_string(),
            },
        );
        let json = serde_json::to_string(&msg).expect("serialize");
        let back: ChatMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(
            back.provider_continuation
                .as_ref()
                .and_then(ProviderContinuation::anthropic_signature),
            Some("sig_abc123_encrypted_blob")
        );
        assert_eq!(back.content, "Step 3 lives.");
    }

    #[test]
    fn provider_continuation_defaults_to_none() {
        // Backward compat: messages saved before Step 3 won't have the
        // field. Serde default kicks in — None — and deserialize
        // succeeds without errors.
        let pre_step3_json = r#"{
            "role": "Assistant",
            "content": "hello",
            "timestamp": "2026-04-16T12:00:00-04:00"
        }"#;
        let msg: ChatMessage = serde_json::from_str(pre_step3_json).expect("backward compat");
        assert!(msg.provider_continuation.is_none());
    }

    #[test]
    fn meta_encrypted_continuation_survives_persistence_redaction_byte_exact() {
        let original = "eyJopaque.reasoning.payload";
        let message = ChatMessage::assistant("done").with_provider_continuation(
            ProviderContinuation::MetaResponses {
                output: vec![MetaResponseItem::from_wire(serde_json::json!({
                    "type": "reasoning",
                    "id": "rs_1",
                    "summary": [],
                    "encrypted_content": original,
                }))],
            },
        );
        let mut persisted = serde_json::to_value(message).unwrap();
        crate::utils::redact_json(&mut persisted);
        let restored: ChatMessage = serde_json::from_value(persisted).unwrap();
        let output = restored
            .provider_continuation
            .as_ref()
            .and_then(ProviderContinuation::meta_output)
            .unwrap();
        assert_eq!(output[0].to_wire()["encrypted_content"], original);
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
        let kind: ChatMessageKind = serde_json::from_str("\"some_future_kind\"").expect("tolerant");
        assert_eq!(kind, ChatMessageKind::Unknown);
        assert_ne!(kind, ChatMessageKind::Normal);
    }

    #[test]
    fn continuation_kinds_round_trip_through_serde() {
        // The stitch markers must survive session save/load: a reloaded
        // transcript stitches exactly like the live one did.
        for kind in [
            ChatMessageKind::Continuation,
            ChatMessageKind::RecoveryNudge,
        ] {
            let json = serde_json::to_string(&kind).unwrap();
            let back: ChatMessageKind = serde_json::from_str(&json).unwrap();
            assert_eq!(back, kind);
        }
        // And the snake_case wire form is stable.
        assert_eq!(
            serde_json::to_string(&ChatMessageKind::Continuation).unwrap(),
            "\"continuation\""
        );
        assert_eq!(
            serde_json::to_string(&ChatMessageKind::RecoveryNudge).unwrap(),
            "\"recovery_nudge\""
        );
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
        let usage = TokenUsage::provider(100, 50);

        let response = ModelResponse {
            content: "Hello, world!".to_string(),
            usage: Some(usage),
            model_name: "ollama/tinyllama".to_string(),
            thinking: None,
            tool_calls: None,
            stop_reason: None,
            provider_continuation: None,
        };

        assert_eq!(response.content, "Hello, world!");
        assert!(response.usage.is_some());
        assert_eq!(response.model_name, "ollama/tinyllama");
        assert_eq!(response.usage.unwrap().total_tokens(), 150);
        assert!(response.tool_calls.is_none());
    }
}
