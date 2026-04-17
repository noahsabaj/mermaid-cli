//! Application coordinator
//!
//! Thin coordinator that composes state modules. All state is delegated to
//! focused modules in src/tui/state/.

use std::collections::VecDeque;
use std::sync::Arc;
use tokio::task::JoinHandle;

use super::state::{
    AppState, AttachmentState, ConversationState, ErrorEntry, ErrorSeverity, GenerationStatus,
    InputBuffer, ModelState, OperationState, StatusState, UIState,
};
use crate::constants::{MAX_RESPONSE_CHARS, UI_ERROR_LOG_MAX_SIZE};
use crate::models::{ChatMessage, MessageRole, Model, ModelConfig, StreamCallback, StreamEvent};
use crate::session::{ConversationHistory, ConversationManager};
use crate::utils::MutexExt;

/// Truncation marker appended to the response buffer once the size cap is
/// hit. Public to the module so tests can assert against it.
const TRUNCATION_MARKER: &str = "\n\n[TRUNCATED: Response exceeded size limit]\n";

/// Append `chunk` to `buf`, enforcing `cap` bytes (char-boundary safe). Once
/// the cap is tripped (`*truncated` set to `true`), subsequent calls become
/// no-ops — preventing the O(n)-per-chunk re-truncation and duplicated
/// markers that the original `push_response` exhibited under runaway model
/// output. Returns `true` if this call performed the truncation.
fn push_with_cap(buf: &mut String, truncated: &mut bool, chunk: &str, cap: usize) -> bool {
    if *truncated {
        return false;
    }
    buf.push_str(chunk);
    if buf.len() > cap {
        let end = buf.floor_char_boundary(cap);
        buf.truncate(end);
        buf.push_str(TRUNCATION_MARKER);
        *truncated = true;
        true
    } else {
        false
    }
}

/// Application state coordinator
pub struct App {
    /// User input buffer
    pub input: InputBuffer,
    /// Is the app running?
    pub running: bool,
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
    /// MCP tool definitions in Ollama JSON format (injected at startup or background init)
    pub mcp_tools: Vec<serde_json::Value>,
    /// Background MCP server initialization task.
    /// Polled in the event loop; when done, mcp_tools and global manager are set.
    pub mcp_init_task: Option<JoinHandle<McpInitResult>>,
    /// Project instructions auto-loaded from MERMAID.md (Step 5h).
    /// `None` when no MERMAID.md exists in the bounded walk from cwd.
    /// Refreshed before every model call by `loop_coordinator::call_model`.
    pub instructions: Option<crate::app::instructions::LoadedInstructions>,
}

/// Result of background MCP server initialization
pub struct McpInitResult {
    pub tools: Vec<serde_json::Value>,
    pub manager: Option<Arc<crate::mcp::McpServerManager>>,
}

impl App {
    /// Create a new app instance
    pub fn new(model: Box<dyn Model>, model_id: String, base_config: ModelConfig) -> Self {
        let working_dir = std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| ".".to_string());

        // Initialize model state
        let model_state = ModelState::new(model, model_id, base_config);

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

        // Initialize input buffer with persisted history
        let mut input = InputBuffer::new();
        input.load_history(input_history);

        // Initialize UIState
        let ui_state = UIState::new();

        // Initialize ConversationState with conversation management
        let session_state =
            ConversationState::with_conversation(conversation_manager, current_conversation);

        Self {
            input,
            running: true,
            working_dir,
            error_log: VecDeque::new(),
            app_state: AppState::Idle,
            model_state,
            ui_state,
            session_state,
            operation_state: OperationState::new(),
            status_state: StatusState::new(),
            attachment_state: AttachmentState::new(),
            mcp_tools: Vec::new(),
            mcp_init_task: None,
            instructions: None,
        }
    }

    /// Build a ModelConfig with MCP tools included.
    pub fn build_model_config(&self) -> crate::models::ModelConfig {
        let mut config = self.model_state.build_config();
        config.mcp_tools = self.mcp_tools.clone();
        config
    }

    /// Poll for completed MCP background initialization (non-blocking).
    ///
    /// Returns immediately if init is still in progress or already done.
    /// When init completes, sets mcp_tools on self and registers the global
    /// MCP manager so tool calls can be dispatched.
    ///
    /// Called from both the main event loop and the agent loop so that MCP
    /// tools become available to the model as soon as servers are ready,
    /// even mid-agent-loop.
    pub async fn poll_mcp_init(&mut self) {
        let ready = self.mcp_init_task.as_ref().is_some_and(|t| t.is_finished());
        if !ready {
            return;
        }
        if let Some(task) = self.mcp_init_task.take()
            && let Ok(result) = task.await
        {
            if !result.tools.is_empty() {
                self.mcp_tools = result.tools;
            }
            if let Some(manager) = result.manager {
                crate::agents::set_mcp_manager(manager);
            }
        }
        // Mark complete whether the task succeeded or panicked, so waiters unblock
        crate::agents::mark_mcp_init_complete();
    }

    // ===== Message Management =====

    /// Add a message to the chat (extracts thinking blocks automatically)
    pub fn add_message(&mut self, role: MessageRole, content: String) {
        self.add_message_with_images(role, content, None);
    }

    /// Add a message with optional image attachments
    pub fn add_message_with_images(
        &mut self,
        role: MessageRole,
        content: String,
        images: Option<Vec<String>>,
    ) {
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
    pub fn add_tool_result(&mut self, tool_call_id: String, tool_name: String, content: String) {
        let message = ChatMessage::tool(tool_call_id, tool_name, content);
        self.commit_message(message);
    }

    /// Commit a message to session state and conversation history
    pub fn commit_message(&mut self, message: ChatMessage) {
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
        self.log_error(ErrorEntry::with_context(
            severity,
            msg.into(),
            context.into(),
        ));
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

    /// Spawn title generation as a background task (non-blocking).
    /// Returns a JoinHandle the caller can poll with `is_finished()`.
    pub fn spawn_title_generation(&self) -> Option<tokio::task::JoinHandle<Option<String>>> {
        if self.session_state.conversation_title.is_some() || self.session_state.messages.len() < 2
        {
            return None;
        }

        let mut summary = String::new();
        for msg in self
            .session_state
            .messages
            .iter()
            .filter(|m| matches!(m.role, MessageRole::User | MessageRole::Assistant))
            .take(4)
        {
            let role = if msg.role == MessageRole::User {
                "User"
            } else {
                "Assistant"
            };
            summary.push_str(&format!(
                "{}: {}\n\n",
                role,
                msg.content.chars().take(200).collect::<String>()
            ));
        }

        let model = self.model_state.model.clone();
        let mut config = self.build_model_config();
        // Title generation is a quick utility call — no reasoning needed.
        config.reasoning = crate::models::ReasoningLevel::None;

        Some(tokio::spawn(async move {
            let prompt = format!(
                "Based on this conversation, generate a short, descriptive title (2-4 words maximum, no quotes):\n\n{}\n\nTitle:",
                summary
            );
            // `std::sync::Mutex` (via MutexExt's `lock_mut_safe`) matches
            // the rest of the codebase and avoids the `try_lock` race the
            // earlier `tokio::sync::Mutex` had — accumulator never drops
            // a chunk because the lock was momentarily contended. The
            // closure stays `Send + Sync` because `std::sync::Mutex<String>`
            // is both.
            let buf = Arc::new(std::sync::Mutex::new(String::new()));
            let buf_clone = Arc::clone(&buf);
            let callback: StreamCallback = Arc::new(move |event| {
                if let StreamEvent::Text(chunk) = event {
                    buf_clone.lock_mut_safe().push_str(&chunk);
                }
                // Reasoning / tool calls / done are irrelevant for title
                // generation — we only want the model's literal text reply.
            });

            let model = model.read().await;
            if model
                .chat(&[ChatMessage::user(prompt)], &config, Some(callback))
                .await
                .is_ok()
            {
                let raw = buf.lock_mut_safe();
                let title: String = raw
                    .lines()
                    .next()
                    .unwrap_or(&raw)
                    .trim()
                    .trim_matches(|c| c == '"' || c == '\'' || c == '.' || c == ',')
                    .chars()
                    .take(50)
                    .collect();
                if !title.is_empty() {
                    return Some(title);
                }
            }
            None
        }))
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

    /// Filter and prepare messages for model API calls.
    /// Includes User, Assistant, and Tool messages for proper agent loop.
    /// Injects timestamp context into User messages for the model's temporal awareness.
    /// Step 5f Wave 5: prunes stale screenshots via `prune_stale_screenshots`.
    fn prepare_api_messages(&self) -> Vec<ChatMessage> {
        let prepared: Vec<ChatMessage> = self
            .session_state
            .messages
            .iter()
            .filter(|msg| {
                msg.role == MessageRole::User
                    || msg.role == MessageRole::Assistant
                    || msg.role == MessageRole::Tool
            })
            .map(|msg| {
                if msg.role == MessageRole::User {
                    let ts = msg.timestamp.format("%Y-%m-%d %H:%M:%S %Z").to_string();
                    let mut m = msg.clone();
                    m.content = format!("[Sent at: {}]\n{}", ts, m.content);
                    m
                } else {
                    msg.clone()
                }
            })
            .collect();

        prune_stale_screenshots(prepared, crate::constants::MAX_RETAINED_SCREENSHOTS)
    }

    /// Build message history for model API calls (all messages, no truncation)
    pub fn build_message_history(&self) -> Vec<ChatMessage> {
        self.prepare_api_messages()
    }

    pub fn build_managed_message_history(
        &self,
        max_context_tokens: usize,
        reserve_tokens: usize,
    ) -> Vec<ChatMessage> {
        use crate::utils::Tokenizer;

        let tokenizer = Tokenizer::new(&self.model_state.model_name);
        let available_tokens = max_context_tokens.saturating_sub(reserve_tokens);

        let all_messages = self.prepare_api_messages();

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
        self.session_state.cumulative_tokens = conversation.total_tokens.unwrap_or(0);
        self.session_state.conversation_title = Some(conversation.title.clone());
        self.session_state.messages = conversation.messages.clone();
        self.session_state.current_conversation = Some(conversation);
        self.set_status("Conversation loaded");
    }

    pub fn save_conversation(&mut self) -> anyhow::Result<()> {
        if let Some(ref manager) = self.session_state.conversation_manager
            && let Some(ref mut conv) = self.session_state.current_conversation
        {
            conv.messages = self.session_state.messages.clone();
            conv.total_tokens = Some(self.session_state.cumulative_tokens);
            manager.save_conversation(conv)?;
            self.set_status("Conversation saved");
        }
        Ok(())
    }

    pub fn auto_save_conversation(&mut self) {
        if self.session_state.messages.is_empty() {
            return;
        }
        if let Some(ref manager) = self.session_state.conversation_manager
            && let Some(ref mut conv) = self.session_state.current_conversation
        {
            conv.messages = self.session_state.messages.clone();
            conv.total_tokens = Some(self.session_state.cumulative_tokens);
            let conv_clone = conv.clone();
            let manager_clone = manager.clone();
            tokio::task::spawn_blocking(move || {
                if let Err(e) = manager_clone.save_conversation(&conv_clone) {
                    tracing::warn!("Failed to auto-save conversation: {}", e);
                }
            });
        }
    }

    // ===== Generation State Transitions =====

    pub fn start_generation(&mut self, abort_handle: tokio::task::AbortHandle) {
        self.app_state = AppState::Generating {
            status: GenerationStatus::Sending,
            start_time: std::time::Instant::now(),
            tokens_received: 0,
            abort_handle: Some(abort_handle),
            response_buffer: String::with_capacity(8192),
            response_truncated: false,
        };
    }

    /// Update the abort handle for a new model call within the same turn.
    /// Keeps the existing start_time and token count (cumulative for the turn).
    pub fn update_abort_handle(&mut self, abort_handle: tokio::task::AbortHandle) {
        if let AppState::Generating {
            abort_handle: ref mut existing,
            ..
        } = self.app_state
        {
            *existing = Some(abort_handle);
        }
    }

    /// Reset status to Sending for a new model call within the same turn.
    pub fn transition_to_sending(&mut self) {
        if let AppState::Generating { status, .. } = &mut self.app_state {
            *status = GenerationStatus::Sending;
        }
    }

    pub fn transition_to_thinking(&mut self) {
        if let AppState::Generating { status, .. } = &mut self.app_state {
            *status = GenerationStatus::Thinking;
        }
    }

    pub fn transition_to_streaming(&mut self) {
        if let AppState::Generating { status, .. } = &mut self.app_state {
            *status = GenerationStatus::Streaming;
        }
    }

    /// Add tokens from a completed model call (accumulates across the turn)
    pub fn set_final_tokens(&mut self, count: usize) {
        if let AppState::Generating {
            tokens_received, ..
        } = &mut self.app_state
        {
            *tokens_received += count;
        }
        self.session_state.add_tokens(count);
    }

    pub fn stop_generation(&mut self) {
        self.app_state = AppState::Idle;
    }

    pub fn abort_generation(&mut self) -> (Option<tokio::task::AbortHandle>, String) {
        if let AppState::Generating {
            abort_handle,
            response_buffer,
            ..
        } = &mut self.app_state
        {
            let handle = abort_handle.take();
            let buffer = std::mem::take(response_buffer);
            self.app_state = AppState::Idle;
            (handle, buffer)
        } else {
            (None, String::new())
        }
    }

    // ===== Response Buffer Accessors =====

    /// Append text to the response buffer. No-op if not generating.
    /// Enforces MAX_RESPONSE_CHARS size limit; once tripped, subsequent calls
    /// are silently dropped so we don't pay O(n) per chunk re-truncating
    /// (and don't emit duplicate `[TRUNCATED…]` markers).
    pub fn push_response(&mut self, text: &str) {
        let mut just_truncated = false;
        if let AppState::Generating {
            response_buffer,
            response_truncated,
            ..
        } = &mut self.app_state
        {
            just_truncated = push_with_cap(
                response_buffer,
                response_truncated,
                text,
                MAX_RESPONSE_CHARS,
            );
        }
        if just_truncated {
            self.set_status("[WARNING] Response truncated (size limit reached)");
        }
    }

    /// Get response buffer length (0 if not generating)
    pub fn response_len(&self) -> usize {
        if let AppState::Generating {
            response_buffer, ..
        } = &self.app_state
        {
            response_buffer.len()
        } else {
            0
        }
    }

    /// Take the response buffer, leaving it empty. Returns empty string if not generating.
    /// Also clears the `response_truncated` flag so the next model call starts fresh.
    pub fn take_response(&mut self) -> String {
        if let AppState::Generating {
            response_buffer,
            response_truncated,
            ..
        } = &mut self.app_state
        {
            *response_truncated = false;
            std::mem::take(response_buffer)
        } else {
            String::new()
        }
    }

    /// Clear the response buffer (for per-model-call reset within a turn)
    /// and the truncated flag so the new call's buffer is fresh.
    pub fn clear_response(&mut self) {
        if let AppState::Generating {
            response_buffer,
            response_truncated,
            ..
        } = &mut self.app_state
        {
            response_buffer.clear();
            *response_truncated = false;
        }
    }
}

/// Drop image attachments from all but the most recent `keep` messages
/// that have images. Older messages keep their text content, with a
/// placeholder appended noting how many turns ago the image was — so
/// the model knows what was elided rather than wondering why an action
/// "happened with no visible result." Each click in computer-use mode
/// auto-attaches a ~1k-token base64 PNG; without pruning, a 10-click
/// loop bloats history by ~10k tokens of stale visuals.
pub(crate) fn prune_stale_screenshots(
    mut messages: Vec<ChatMessage>,
    keep: usize,
) -> Vec<ChatMessage> {
    let image_indices: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter_map(|(i, m)| m.images.as_ref().filter(|imgs| !imgs.is_empty()).map(|_| i))
        .collect();
    let keep_threshold = image_indices.len().saturating_sub(keep);
    for (rank, idx) in image_indices.iter().enumerate() {
        if rank < keep_threshold {
            let turns_ago = image_indices.len() - rank;
            let placeholder = format!(
                "\n[screenshot from {} turns ago — dropped from context to save tokens, see latest]",
                turns_ago
            );
            messages[*idx].images = None;
            messages[*idx].content.push_str(&placeholder);
        }
    }
    messages
}

#[cfg(test)]
mod tests {
    use super::{TRUNCATION_MARKER, prune_stale_screenshots, push_with_cap};
    use crate::models::ChatMessage;

    #[test]
    fn push_with_cap_under_limit_appends_normally() {
        let mut buf = String::new();
        let mut truncated = false;
        assert!(!push_with_cap(&mut buf, &mut truncated, "hello", 100));
        assert!(!push_with_cap(&mut buf, &mut truncated, " world", 100));
        assert_eq!(buf, "hello world");
        assert!(!truncated);
    }

    #[test]
    fn push_with_cap_truncates_once_and_short_circuits() {
        let mut buf = String::new();
        let mut truncated = false;
        let cap = 10;
        let big = "a".repeat(50);

        // First push trips the cap.
        assert!(push_with_cap(&mut buf, &mut truncated, &big, cap));
        assert!(truncated);
        assert!(buf.starts_with(&"a".repeat(10)));
        assert!(buf.ends_with(TRUNCATION_MARKER));
        let len_after_first = buf.len();
        let marker_count_first = buf.matches(TRUNCATION_MARKER).count();
        assert_eq!(marker_count_first, 1);

        // Subsequent pushes must be no-ops — buffer unchanged, no extra marker.
        assert!(!push_with_cap(&mut buf, &mut truncated, &big, cap));
        assert!(!push_with_cap(&mut buf, &mut truncated, "more stuff", cap));
        assert!(!push_with_cap(&mut buf, &mut truncated, &big, cap));
        assert_eq!(buf.len(), len_after_first);
        assert_eq!(buf.matches(TRUNCATION_MARKER).count(), 1);
    }

    #[test]
    fn push_with_cap_respects_char_boundary_for_cjk() {
        let mut buf = String::new();
        let mut truncated = false;
        // Each 你 is 3 bytes. cap=4 lands inside the second 你; floor must
        // truncate to 3 bytes (one full character) before appending the marker.
        let chunk = "你你你你".to_string();
        assert!(push_with_cap(&mut buf, &mut truncated, &chunk, 4));
        // Truncated content should be exactly "你" (3 bytes), then the marker.
        let body = &buf[..buf.find('\n').unwrap()];
        assert_eq!(body, "你");
        assert!(buf.ends_with(TRUNCATION_MARKER));
    }

    /// Step 5f Wave 5: out of N screenshot-bearing messages, keep only
    /// the last K — older messages have their `images` set to None.
    #[test]
    fn prune_stale_screenshots_keeps_only_last_3() {
        let mk = |i: i32, has_img: bool| {
            let mut m = ChatMessage::user(format!("msg {}", i));
            if has_img {
                m = m.with_images(vec![format!("base64-data-{}", i)]);
            }
            m
        };
        let msgs = vec![
            mk(0, true),
            mk(1, true),
            mk(2, true),
            mk(3, true),
            mk(4, true),
        ];
        let pruned = prune_stale_screenshots(msgs, 3);
        // Last 3 (indices 2, 3, 4) keep images.
        assert!(pruned[0].images.is_none());
        assert!(pruned[1].images.is_none());
        assert!(pruned[2].images.is_some());
        assert!(pruned[3].images.is_some());
        assert!(pruned[4].images.is_some());
    }

    /// Step 5f Wave 5: dropped screenshots get a placeholder explaining
    /// to the model that the image was elided. Without this the model
    /// might think the screenshot tool failed.
    #[test]
    fn prune_stale_screenshots_appends_placeholder_for_dropped() {
        let mk = |i: i32| {
            ChatMessage::user(format!("msg {}", i)).with_images(vec![format!("data-{}", i)])
        };
        let msgs = vec![mk(0), mk(1), mk(2), mk(3)];
        let pruned = prune_stale_screenshots(msgs, 2);
        // First 2 are pruned; their content should mention "screenshot from N turns ago"
        assert!(pruned[0].content.contains("screenshot from"));
        assert!(pruned[0].content.contains("turns ago"));
        assert!(pruned[1].content.contains("screenshot from"));
        // Last 2 retain images, content unchanged.
        assert!(!pruned[2].content.contains("screenshot from"));
        assert!(!pruned[3].content.contains("screenshot from"));
    }

    #[test]
    fn prune_stale_screenshots_no_op_when_under_keep_threshold() {
        let mk = |i: i32| {
            ChatMessage::user(format!("msg {}", i)).with_images(vec![format!("data-{}", i)])
        };
        let msgs = vec![mk(0), mk(1)]; // only 2 images, keep = 3 → all retained
        let pruned = prune_stale_screenshots(msgs, 3);
        assert!(pruned[0].images.is_some());
        assert!(pruned[1].images.is_some());
    }
}
