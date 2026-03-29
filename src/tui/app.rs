//! Application coordinator
//!
//! Thin coordinator that composes state modules. All state is delegated to
//! focused modules in src/tui/state/.

use std::collections::VecDeque;
use std::sync::Arc;
use tracing::warn;

use super::state::{
    AppState, AttachmentState, ConversationState, ErrorEntry, ErrorSeverity, GenerationStatus,
    InputBuffer, ModelState, OperationState, StatusState, UIState,
};
use super::theme::Theme;
use super::widgets::{ChatState, InputState};
use crate::constants::UI_ERROR_LOG_MAX_SIZE;
use crate::models::{ChatMessage, MessageRole, Model, StreamCallback};
use crate::session::{ConversationHistory, ConversationManager};

/// Application state coordinator
pub struct App {
    /// User input buffer
    pub input: InputBuffer,
    /// Is the app running?
    pub running: bool,
    /// Current model response (for streaming)
    pub current_response: String,
    /// Current working directory
    pub working_dir: String,
    /// Error log - keeps last N errors for visibility
    pub error_log: VecDeque<ErrorEntry>,
    /// State machine for application lifecycle
    pub app_state: AppState,

    /// Model state - LLM configuration
    pub model_state: ModelState,
    /// UI state - visual presentation and widget states
    pub ui_state: UIState,
    /// Session state - conversation history and persistence
    pub session_state: ConversationState,
    /// Operation state - file reading and tool calls
    pub operation_state: OperationState,
    /// Status state - UI status messages
    pub status_state: StatusState,
    /// Attachment state - pending image attachments
    pub attachment_state: AttachmentState,
}

impl App {
    /// Create a new app instance
    pub fn new(model: Box<dyn Model>, model_id: String) -> Self {
        let working_dir = std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| ".".to_string());

        // Initialize model state
        let model_state = ModelState::new(model, model_id);

        // Initialize conversation manager for the current directory
        let conversation_manager = ConversationManager::new(&working_dir).ok();
        let current_conversation = conversation_manager
            .as_ref()
            .map(|_| ConversationHistory::new(working_dir.clone(), model_state.model_name.clone()));

        // Load input history from conversation if available
        let input_history: std::collections::VecDeque<String> = current_conversation
            .as_ref()
            .map(|conv| conv.input_history.clone())
            .unwrap_or_default();

        // Initialize UIState
        let ui_state = UIState {
            chat_state: ChatState::new(),
            input_state: InputState::new(),
            theme: Theme::dark(),
            selected_message: None,
            attachment_focused: false,
            selected_attachment: 0,
            attachment_area_y: None,
        };

        // Initialize ConversationState with conversation management
        let session_state = ConversationState::with_conversation(
            conversation_manager,
            current_conversation,
            input_history,
        );

        Self {
            input: InputBuffer::new(),
            running: true,
            current_response: String::with_capacity(8192),
            working_dir,
            error_log: VecDeque::new(),
            app_state: AppState::Idle,
            model_state,
            ui_state,
            session_state,
            operation_state: OperationState::new(),
            status_state: StatusState::new(),
            attachment_state: AttachmentState::new(),
        }
    }

    // ===== Compatibility shims for old field access =====
    // These will be removed as callers are updated

    /// Get cursor position (compatibility shim)
    pub fn cursor_position(&self) -> usize {
        self.input.cursor_position
    }

    /// Set cursor position (compatibility shim)
    pub fn set_cursor_position(&mut self, pos: usize) {
        self.input.cursor_position = pos;
    }

    // ===== Message Management =====

    /// Add a message to the chat (extracts thinking blocks automatically)
    pub fn add_message(&mut self, role: MessageRole, content: String) {
        let mut message = match role {
            MessageRole::User => ChatMessage::user(content),
            MessageRole::Assistant => ChatMessage::assistant(content),
            MessageRole::System => ChatMessage::system(content),
            MessageRole::Tool => ChatMessage::tool("", "", content),
        };
        // Extract thinking from content
        let (thinking, answer) = ChatMessage::extract_thinking(&message.content);
        message.content = answer;
        message.thinking = thinking;
        self.commit_message(message);
    }

    /// Add a message with image attachments
    pub fn add_message_with_images(&mut self, role: MessageRole, content: String, images: Option<Vec<String>>) {
        let mut message = match role {
            MessageRole::User => ChatMessage::user(content),
            MessageRole::Assistant => ChatMessage::assistant(content),
            MessageRole::System => ChatMessage::system(content),
            MessageRole::Tool => ChatMessage::tool("", "", content),
        };
        let (thinking, answer) = ChatMessage::extract_thinking(&message.content);
        message.content = answer;
        message.thinking = thinking;
        if let Some(imgs) = images {
            message = message.with_images(imgs);
        }
        self.commit_message(message);
    }

    /// Add an assistant message with tool_calls attached
    pub fn add_assistant_message_with_tool_calls(
        &mut self,
        content: String,
        tool_calls: Vec<crate::models::ToolCall>,
    ) {
        let mut message = ChatMessage::assistant(content).with_tool_calls(tool_calls);
        let (thinking, answer) = ChatMessage::extract_thinking(&message.content);
        message.content = answer;
        message.thinking = thinking;
        self.commit_message(message);
    }

    /// Add a tool result message
    pub fn add_tool_result(
        &mut self,
        tool_call_id: String,
        tool_name: String,
        content: String,
    ) {
        let message = ChatMessage::tool(tool_call_id, tool_name, content);
        self.commit_message(message);
    }

    /// Commit a message to session state and conversation history
    fn commit_message(&mut self, message: ChatMessage) {
        self.session_state.messages.push(message.clone());
        if let Some(ref mut conv) = self.session_state.current_conversation {
            conv.add_messages(&[message]);
        }
    }

    /// Clear the input buffer
    pub fn clear_input(&mut self) {
        self.input.clear();
    }

    // ===== Status Management =====

    /// Set status message
    pub fn set_status(&mut self, message: impl Into<String>) {
        self.status_state.set(message);
    }

    /// Clear status message
    pub fn clear_status(&mut self) {
        self.status_state.clear();
    }

    // ===== Error Management =====

    /// Display an error consistently across the UI
    pub fn display_error(&mut self, summary: impl Into<String>, detail: impl Into<String>) {
        let summary = summary.into();
        let detail = detail.into();

        self.set_status(format!("[Error] {}", summary));

        if detail.is_empty() {
            self.add_message(MessageRole::System, format!("Error: {}", summary));
        } else {
            self.add_message(MessageRole::System, detail);
        }
    }

    /// Display an error with just a message
    pub fn display_error_simple(&mut self, message: impl Into<String>) {
        let message = message.into();
        self.display_error(message.clone(), message);
    }

    /// Log an error to the error log
    pub fn log_error(&mut self, entry: ErrorEntry) {
        self.status_state.set(entry.display());
        self.error_log.push_back(entry);
        if self.error_log.len() > UI_ERROR_LOG_MAX_SIZE {
            self.error_log.pop_front(); // O(1) instead of O(n)
        }
    }

    /// Log a simple error message
    pub fn log_error_msg(&mut self, severity: ErrorSeverity, msg: impl Into<String>) {
        self.log_error(ErrorEntry::new(severity, msg.into()));
    }

    /// Log error with context
    pub fn log_error_with_context(
        &mut self,
        severity: ErrorSeverity,
        msg: impl Into<String>,
        context: impl Into<String>,
    ) {
        self.log_error(ErrorEntry::with_context(severity, msg.into(), context.into()));
    }

    /// Get recent errors
    pub fn recent_errors(&self, count: usize) -> Vec<&ErrorEntry> {
        self.error_log.iter().rev().take(count).collect()
    }

    // ===== Terminal =====

    /// Set terminal window title
    pub fn set_terminal_title(&self, title: &str) {
        use crossterm::{execute, terminal::SetTitle};
        use std::io::stdout;
        let _ = execute!(stdout(), SetTitle(title));
    }

    // ===== Title Generation =====

    /// Generate conversation title from current messages
    pub async fn generate_conversation_title(&mut self) {
        if self.session_state.conversation_title.is_some() || self.session_state.messages.len() < 2 {
            return;
        }

        let mut conversation_summary = String::new();
        for (i, msg) in self.session_state.messages.iter().take(4).enumerate() {
            let role = match msg.role {
                MessageRole::User => "User",
                MessageRole::Assistant => "Assistant",
                MessageRole::System | MessageRole::Tool => continue,
            };
            conversation_summary.push_str(&format!(
                "{}: {}\n\n",
                role,
                msg.content.chars().take(200).collect::<String>()
            ));
            if i >= 3 { break; }
        }

        let title_prompt = format!(
            "Based on this conversation, generate a short, descriptive title (2-4 words maximum, no quotes):\n\n{}\n\nTitle:",
            conversation_summary
        );

        let messages = vec![ChatMessage::user(title_prompt)];

        let title_string = Arc::new(tokio::sync::Mutex::new(String::new()));
        let title_clone = Arc::clone(&title_string);

        let callback: StreamCallback = Arc::new(move |chunk: &str| {
            if let Ok(mut title) = title_clone.try_lock() {
                title.push_str(chunk);
            }
        });

        let model = self.model_state.model.write().await;
        let config = self.model_state.build_config();

        if model.chat(&messages, &config, Some(callback)).await.is_ok() {
            let final_title = title_string.lock().await;
            let title = final_title.lines().next().unwrap_or(&final_title)
                .trim()
                .trim_matches(|c| c == '"' || c == '\'' || c == '.' || c == ',')
                .chars()
                .take(50)
                .collect::<String>();

            if !title.is_empty() {
                self.session_state.conversation_title = Some(title);
            }
        }
    }

    // ===== Scrolling =====

    pub fn scroll_up(&mut self, amount: u16) {
        self.ui_state.chat_state.scroll_up(amount);
    }

    pub fn scroll_down(&mut self, amount: u16) {
        self.ui_state.chat_state.scroll_down(amount);
    }

    // ===== Lifecycle =====

    pub fn quit(&mut self) {
        self.running = false;
    }

    // ===== Message History =====

    /// Build message history for model API calls
    /// Includes User, Assistant, and Tool messages (for proper agent loop)
    pub fn build_message_history(&self) -> Vec<ChatMessage> {
        self.session_state.messages
            .iter()
            .filter(|msg| {
                msg.role == MessageRole::User
                    || msg.role == MessageRole::Assistant
                    || msg.role == MessageRole::Tool
            })
            .cloned()
            .collect()
    }

    pub fn build_managed_message_history(
        &self,
        max_context_tokens: usize,
        reserve_tokens: usize,
    ) -> Vec<ChatMessage> {
        use crate::utils::Tokenizer;

        let tokenizer = Tokenizer::new(&self.model_state.model_name);
        let available_tokens = max_context_tokens.saturating_sub(reserve_tokens);

        // Include User, Assistant, and Tool messages for proper agent loop
        let all_messages: Vec<ChatMessage> = self
            .session_state
            .messages
            .iter()
            .filter(|msg| {
                msg.role == MessageRole::User
                    || msg.role == MessageRole::Assistant
                    || msg.role == MessageRole::Tool
            })
            .cloned()
            .collect();

        if all_messages.is_empty() {
            return Vec::new();
        }

        let messages_for_counting: Vec<(String, String)> = all_messages
            .iter()
            .map(|msg| {
                let role = match msg.role {
                    MessageRole::User => "user",
                    MessageRole::Assistant => "assistant",
                    MessageRole::System => "system",
                    MessageRole::Tool => "tool",
                };
                (role.to_string(), msg.content.clone())
            })
            .collect();

        let total_tokens = tokenizer
            .count_chat_tokens(&messages_for_counting)
            .unwrap_or_else(|_| all_messages.iter().map(|m| m.content.len() / 4).sum());

        if total_tokens <= available_tokens {
            return all_messages;
        }

        let mut kept_messages = Vec::new();
        let mut current_tokens = 0;

        for msg in all_messages.iter().rev() {
            let msg_text = vec![(
                match msg.role {
                    MessageRole::User => "user",
                    MessageRole::Assistant => "assistant",
                    MessageRole::System => "system",
                    MessageRole::Tool => "tool",
                }
                .to_string(),
                msg.content.clone(),
            )];

            let msg_tokens = tokenizer
                .count_chat_tokens(&msg_text)
                .unwrap_or(msg.content.len() / 4);

            if current_tokens + msg_tokens <= available_tokens {
                kept_messages.push(msg.clone());
                current_tokens += msg_tokens;
            } else if kept_messages.len() < 2 {
                kept_messages.push(msg.clone());
                break;
            } else {
                break;
            }
        }

        kept_messages.reverse();
        kept_messages
    }

    // ===== Conversation Persistence =====

    pub fn load_conversation(&mut self, conversation: ConversationHistory) {
        self.session_state.messages = conversation.messages.clone();
        self.session_state.current_conversation = Some(conversation);
        self.set_status("Conversation loaded");
    }

    pub fn save_conversation(&mut self) -> anyhow::Result<()> {
        if let Some(ref manager) = self.session_state.conversation_manager
            && let Some(ref mut conv) = self.session_state.current_conversation {
                conv.messages = self.session_state.messages.clone();
                manager.save_conversation(conv)?;
                self.set_status("Conversation saved");
            }
        Ok(())
    }

    pub fn auto_save_conversation(&mut self) {
        if self.session_state.messages.is_empty() {
            return;
        }
        if let Err(e) = self.save_conversation() {
            warn!("Failed to auto-save conversation: {}", e);
        }
    }

    // ===== Generation State Transitions =====

    pub fn start_generation(&mut self, abort_handle: tokio::task::AbortHandle) {
        // Clear accumulated tool calls from any previous generation
        self.operation_state.accumulated_tool_calls.clear();

        self.app_state = AppState::Generating {
            status: GenerationStatus::Sending,
            start_time: std::time::Instant::now(),
            tokens_received: 0,
            abort_handle: Some(abort_handle),
        };
    }

    pub fn transition_to_thinking(&mut self) {
        if let AppState::Generating { start_time, tokens_received, ref abort_handle, .. } = self.app_state {
            self.app_state = AppState::Generating {
                status: GenerationStatus::Thinking,
                start_time,
                tokens_received,
                abort_handle: abort_handle.clone(),
            };
        }
    }

    pub fn transition_to_streaming(&mut self) {
        if let AppState::Generating { start_time, tokens_received, ref abort_handle, .. } = self.app_state {
            self.app_state = AppState::Generating {
                status: GenerationStatus::Streaming,
                start_time,
                tokens_received,
                abort_handle: abort_handle.clone(),
            };
        }
    }

    /// Set the final token count from Ollama's actual response
    pub fn set_final_tokens(&mut self, count: usize) {
        if let AppState::Generating { status, start_time, ref abort_handle, .. } = self.app_state {
            self.app_state = AppState::Generating {
                status,
                start_time,
                tokens_received: count,
                abort_handle: abort_handle.clone(),
            };
            self.session_state.add_tokens(count);
        }
    }

    pub fn stop_generation(&mut self) {
        self.app_state = AppState::Idle;
    }

    pub fn abort_generation(&mut self) -> Option<tokio::task::AbortHandle> {
        if let AppState::Generating { abort_handle, .. } = &mut self.app_state {
            let handle = abort_handle.take();
            self.app_state = AppState::Idle;
            handle
        } else {
            None
        }
    }

}
