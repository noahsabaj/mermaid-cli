use super::mode::OperationMode;
use super::theme::Theme;
use super::widgets::{ChatState, InputState};
use crate::agents::{AgentAction, ModeAwareExecutor, Plan};
use crate::constants::{UI_DEFAULT_VIEWPORT_HEIGHT, UI_ERROR_LOG_MAX_SIZE, UI_STATUS_MESSAGE_THRESHOLD};
use crate::context::ContextManager;
use crate::models::{ChatMessage, MessageRole, Model, ModelConfig, ProjectContext, StreamCallback};
use crate::session::{ConversationHistory, ConversationManager};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;

/// Generation status for the status line
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenerationStatus {
    /// Not currently generating
    Idle,
    /// Message sent upstream, waiting for model to start responding
    Sending,
    /// Model is loading/initializing (before first token)
    Initializing,
    /// Waiting for first token from model (thinking/reasoning)
    Thinking,
    /// Actively receiving and displaying tokens
    Streaming,
}

impl GenerationStatus {
    pub fn display_text(&self) -> &str {
        match self {
            GenerationStatus::Idle => "Idle",
            GenerationStatus::Sending => "Sending",
            GenerationStatus::Initializing => "Initializing",
            GenerationStatus::Thinking => "Thinking",
            GenerationStatus::Streaming => "Streaming",
        }
    }
}

/// Error severity level
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorSeverity {
    /// Informational (not really an error)
    Info,
    /// Warning - operation completed but with issues
    Warning,
    /// Error - operation failed
    Error,
    /// Security error - action was rejected
    Security,
}

impl ErrorSeverity {
    pub fn display(&self) -> &str {
        match self {
            ErrorSeverity::Info => "INFO",
            ErrorSeverity::Warning => "WARN",
            ErrorSeverity::Error => "ERROR",
            ErrorSeverity::Security => "SECURITY",
        }
    }

    pub fn color(&self) -> &str {
        match self {
            ErrorSeverity::Info => "cyan",
            ErrorSeverity::Warning => "yellow",
            ErrorSeverity::Error => "red",
            ErrorSeverity::Security => "magenta",
        }
    }
}

/// An error entry in the error log
#[derive(Debug, Clone)]
pub struct ErrorEntry {
    pub timestamp: Instant,
    pub severity: ErrorSeverity,
    pub message: String,
    pub context: Option<String>,
}

impl ErrorEntry {
    pub fn new(severity: ErrorSeverity, message: String) -> Self {
        Self {
            timestamp: Instant::now(),
            severity,
            message,
            context: None,
        }
    }

    pub fn with_context(severity: ErrorSeverity, message: String, context: String) -> Self {
        Self {
            timestamp: Instant::now(),
            severity,
            message,
            context: Some(context),
        }
    }

    pub fn display(&self) -> String {
        match &self.context {
            Some(ctx) => format!(
                "[{}] {} - {}",
                self.severity.display(),
                self.message,
                ctx
            ),
            None => format!("[{}] {}", self.severity.display(), self.message),
        }
    }
}

/// Comprehensive state machine for the application lifecycle
/// Impossible states become impossible to represent
#[derive(Debug, Clone)]
pub enum AppState {
    /// Idle - not doing anything, ready for input
    Idle,

    /// Currently generating a response from the model
    Generating {
        status: GenerationStatus,
        start_time: Instant,
        tokens_received: usize,
        abort_handle: Option<tokio::task::AbortHandle>,
    },

    /// Generation complete, waiting for action confirmation
    AwaitingActionConfirmation {
        action: AgentAction,
        executor: ModeAwareExecutor,
    },

    /// Executing an action
    ExecutingAction {
        action: AgentAction,
        start_time: Instant,
    },

    /// Waiting for user to approve a plan
    AwaitingPlanApproval {
        plan: Plan,
        explanation: Option<String>,
    },

    /// Executing a plan step-by-step
    ExecutingPlan {
        plan: Plan,
        current_step: usize,
        start_time: Instant,
    },

    /// Waiting for file read feedback from user
    ReadingFileFeedback {
        intent: Option<String>,
        started_at: Instant,
    },
}

impl AppState {
    /// Get generation status if we're generating
    pub fn generation_status(&self) -> Option<GenerationStatus> {
        match self {
            AppState::Generating { status, .. } => Some(*status),
            _ => None,
        }
    }

    /// Check if we're currently generating
    pub fn is_generating(&self) -> bool {
        matches!(self, AppState::Generating { .. })
    }

    /// Check if we're idle
    pub fn is_idle(&self) -> bool {
        matches!(self, AppState::Idle)
    }

    /// Get generation start time if we're generating
    pub fn generation_start_time(&self) -> Option<Instant> {
        match self {
            AppState::Generating { start_time, .. } => Some(*start_time),
            _ => None,
        }
    }

    /// Get tokens received if we're generating
    pub fn tokens_received(&self) -> Option<usize> {
        match self {
            AppState::Generating { tokens_received, .. } => Some(*tokens_received),
            _ => None,
        }
    }

    /// Get abort handle if we're generating
    pub fn abort_handle(&self) -> Option<&tokio::task::AbortHandle> {
        match self {
            AppState::Generating { abort_handle, .. } => abort_handle.as_ref(),
            _ => None,
        }
    }

    /// Get pending action if awaiting confirmation
    pub fn pending_action(&self) -> Option<&AgentAction> {
        match self {
            AppState::AwaitingActionConfirmation { action, .. } => Some(action),
            _ => None,
        }
    }

    /// Get executor if awaiting confirmation
    pub fn pending_executor(&self) -> Option<&ModeAwareExecutor> {
        match self {
            AppState::AwaitingActionConfirmation { executor, .. } => Some(executor),
            _ => None,
        }
    }

    /// Check if awaiting plan approval
    pub fn is_awaiting_plan_approval(&self) -> bool {
        matches!(self, AppState::AwaitingPlanApproval { .. })
    }

    /// Get active plan if in plan-related state
    pub fn active_plan(&self) -> Option<&Plan> {
        match self {
            AppState::AwaitingPlanApproval { plan, .. } => Some(plan),
            AppState::ExecutingPlan { plan, .. } => Some(plan),
            _ => None,
        }
    }

    /// Check if currently reading file feedback
    pub fn is_reading_file_feedback(&self) -> bool {
        matches!(self, AppState::ReadingFileFeedback { .. })
    }

    /// Get current plan step if executing plan
    pub fn plan_current_step(&self) -> Option<usize> {
        match self {
            AppState::ExecutingPlan { current_step, .. } => Some(*current_step),
            _ => None,
        }
    }

    /// Get action start time if executing
    pub fn action_start_time(&self) -> Option<Instant> {
        match self {
            AppState::ExecutingAction { start_time, .. } => Some(*start_time),
            _ => None,
        }
    }
}

/// UI state - visual presentation and widget states
pub struct UIState {
    /// Chat widget state (scroll, scrolling flag)
    pub chat_state: ChatState,
    /// Input widget state (cursor position for display)
    pub input_state: InputState,
    /// UI theme
    pub theme: Theme,
    /// Selected message index (for navigation)
    pub selected_message: Option<usize>,
}

impl UIState {
    /// Create a new UIState with default values
    pub fn new() -> Self {
        Self {
            chat_state: ChatState::default(),
            input_state: InputState::default(),
            theme: Theme::dark(),
            selected_message: None,
        }
    }
}

/// Session state - conversation history and persistence
pub struct SessionState {
    /// Current chat messages
    pub messages: Vec<ChatMessage>,
    /// Conversation manager for persistence
    pub conversation_manager: Option<ConversationManager>,
    /// Current conversation being tracked
    pub current_conversation: Option<ConversationHistory>,
    /// Input history for arrow key navigation (loaded from session)
    pub input_history: Vec<String>,
    /// Current position in history (None = editing current input, Some(i) = viewing history[i])
    pub history_index: Option<usize>,
    /// Saved input when navigating away from current draft
    pub history_buffer: String,
    /// Context manager for dynamic file tree reloading
    pub context_manager: Option<ContextManager>,
    /// Cumulative token count for the entire conversation
    pub cumulative_tokens: usize,
    /// Auto-generated conversation title (like Claude Code)
    pub conversation_title: Option<String>,
}

impl SessionState {
    /// Create a new SessionState with default values
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
            conversation_manager: None,
            current_conversation: None,
            input_history: Vec::new(),
            history_index: None,
            history_buffer: String::new(),
            context_manager: None,
            cumulative_tokens: 0,
            conversation_title: None,
        }
    }

    /// Create SessionState with conversation management
    pub fn with_conversation(
        conversation_manager: Option<ConversationManager>,
        current_conversation: Option<ConversationHistory>,
        input_history: Vec<String>,
    ) -> Self {
        Self {
            messages: Vec::new(),
            conversation_manager,
            current_conversation,
            input_history,
            history_index: None,
            history_buffer: String::new(),
            context_manager: None,
            cumulative_tokens: 0,
            conversation_title: None,
        }
    }
}

/// Operation state - mode, confirmations, and plan execution
pub struct OperationState {
    /// Current operation mode (Normal, AcceptEdits, PlanMode, BypassAll)
    pub operation_mode: OperationMode,
    /// Flag for confirming destructive operations in BypassAll mode
    pub bypass_confirmed: bool,
    /// Track if FILE_READ feedback is pending
    pub pending_file_read: bool,
    /// Current step index during plan execution (0-based)
    pub plan_execution_index: Option<usize>,
    /// Track if plan mode was active when generation started
    pub plan_mode_active_for_generation: bool,
    /// Status text to show during file reading
    pub reading_file_status: Option<String>,
    /// Current confirmation state
    pub confirmation_state: Option<ConfirmationState>,
}

impl OperationState {
    /// Create a new OperationState with default values
    pub fn new() -> Self {
        Self {
            operation_mode: OperationMode::default(),
            bypass_confirmed: false,
            pending_file_read: false,
            plan_execution_index: None,
            plan_mode_active_for_generation: false,
            reading_file_status: None,
            confirmation_state: None,
        }
    }
}

/// Model state - LLM configuration and identity
pub struct ModelState {
    pub model: Arc<RwLock<Box<dyn Model>>>,
    pub model_id: String,
    pub model_name: String,
}

impl ModelState {
    pub fn new(model: Box<dyn Model>, model_id: String) -> Self {
        let model_name = model.name().to_string();
        Self {
            model: Arc::new(RwLock::new(model)),
            model_id,
            model_name,
        }
    }
}

/// Status state - UI status messages and timing
#[derive(Debug)]
pub struct StatusState {
    pub status_message: Option<String>,
    pub status_timestamp: Option<Instant>,
    pub custom_status: Option<String>,
}

impl StatusState {
    pub fn new() -> Self {
        Self {
            status_message: None,
            status_timestamp: None,
            custom_status: None,
        }
    }
}

/// Application state
pub struct App {
    /// User input buffer
    pub input: String,
    /// Cursor position in the input string
    pub cursor_position: usize,
    /// Is the app running?
    pub running: bool,
    /// Project context
    pub context: ProjectContext,
    /// Current model response (for streaming)
    pub current_response: String,
    /// Current working directory
    pub working_dir: String,
    /// Error log - keeps last N errors for visibility (no more silent failures!)
    pub error_log: Vec<ErrorEntry>,
    /// State machine for application lifecycle
    pub app_state: AppState,

    /// Model state - LLM configuration
    pub model_state: ModelState,
    /// UI state - visual presentation and widget states
    pub ui_state: UIState,
    /// Session state - conversation history and persistence
    pub session_state: SessionState,
    /// Operation state - mode, confirmations, and plan execution
    pub operation_state: OperationState,
    /// Status state - UI status messages
    pub status_state: StatusState,
}

impl App {
    /// Create a new app instance
    pub fn new(model: Box<dyn Model>, context: ProjectContext, model_id: String) -> Self {
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
        let input_history = conversation_manager
            .as_ref()
            .and_then(|_| current_conversation.as_ref())
            .map(|conv| conv.input_history.clone())
            .unwrap_or_default();

        // Initialize UIState
        let ui_state = UIState {
            chat_state: ChatState::new(),
            input_state: InputState::new(),
            theme: Theme::dark(), // Default to dark theme
            selected_message: None,
        };

        // Initialize SessionState with conversation management
        let session_state = SessionState {
            messages: Vec::new(),
            conversation_manager,
            current_conversation,
            input_history,
            history_index: None,
            history_buffer: String::new(),
            context_manager: None,
            cumulative_tokens: 0,
            conversation_title: None,
        };

        // Initialize OperationState
        let operation_state = OperationState::new();

        Self {
            input: String::new(),
            cursor_position: 0,
            running: true,
            context,
            current_response: String::new(),
            working_dir,
            error_log: Vec::new(),
            app_state: AppState::Idle,
            model_state,
            ui_state,
            session_state,
            operation_state,
            status_state: StatusState::new(),
        }
    }

    /// Set the context manager for dynamic context reloading
    pub fn set_context_manager(&mut self, manager: ContextManager) {
        self.session_state.context_manager = Some(manager);
    }

    /// Update context if file tree has changed since last message
    pub async fn reload_context_if_needed(&mut self) -> anyhow::Result<()> {
        if let Some(ref mut manager) = self.session_state.context_manager {
            if manager.reload_if_needed().await? {
                // Context changed, rebuild it
                self.context = manager.build_context();
            }
        }
        Ok(())
    }

    /// Add a message to the chat
    pub fn add_message(&mut self, role: MessageRole, content: String) {
        // Extract thinking blocks if present
        let (thinking, answer_content) = ChatMessage::extract_thinking(&content);

        let message = ChatMessage {
            role,
            content: answer_content,
            timestamp: chrono::Local::now(),
            actions: Vec::new(),
            thinking,
        };
        self.session_state.messages.push(message.clone());

        // Update current conversation
        if let Some(ref mut conv) = self.session_state.current_conversation {
            conv.add_messages(&[message]);
        }

        // Auto-scroll to bottom when adding messages if not manually scrolling
        // Note: Proper scrolling now happens in the main loop with viewport height
        // This is just a placeholder for compatibility
    }

    /// Clear the input buffer
    pub fn clear_input(&mut self) {
        self.input.clear();
        self.cursor_position = 0;
    }

    /// Set status message
    pub fn set_status(&mut self, message: impl Into<String>) {
        self.status_state.status_message = Some(message.into());
    }

    /// Clear status message
    pub fn clear_status(&mut self) {
        self.status_state.status_message = None;
    }

    /// Set terminal window title
    pub fn set_terminal_title(&self, title: &str) {
        use crossterm::{execute, terminal::SetTitle};
        use std::io::stdout;
        let _ = execute!(stdout(), SetTitle(title));
    }

    /// Generate conversation title from current messages
    pub async fn generate_conversation_title(&mut self) {
        // Don't generate if already have a title or less than 2 messages
        if self.session_state.conversation_title.is_some() || self.session_state.messages.len() < 2 {
            return;
        }

        // Build prompt for title generation
        let mut conversation_summary = String::new();
        for (i, msg) in self.session_state.messages.iter().take(4).enumerate() {
            let role = match msg.role {
                MessageRole::User => "User",
                MessageRole::Assistant => "Assistant",
                MessageRole::System => continue, // Skip system messages
            };
            conversation_summary.push_str(&format!("{}: {}\n\n", role, msg.content.chars().take(200).collect::<String>()));
            if i >= 3 { break; } // Only use first 4 messages
        }

        let title_prompt = format!(
            "Based on this conversation, generate a short, descriptive title (2-4 words maximum, no quotes):\n\n{}\n\nTitle:",
            conversation_summary
        );

        // Send title generation request to model
        let messages = vec![ChatMessage {
            role: MessageRole::User,
            content: title_prompt,
            timestamp: chrono::Local::now(),
            actions: Vec::new(),
            thinking: None,
        }];

        // Use the model to generate title
        let title_string = Arc::new(tokio::sync::Mutex::new(String::new()));
        let title_clone = Arc::clone(&title_string);

        let callback: StreamCallback = Arc::new(move |chunk: &str| {
            if let Ok(mut title) = title_clone.try_lock() {
                title.push_str(chunk);
            }
        });

        let mut model = self.model_state.model.write().await;
        let config = ModelConfig::default();

        if let Ok(_) = model.chat(&messages, &self.context, &config, Some(callback)).await {
            let final_title = title_string.lock().await;
            // Clean up title (remove quotes, trim whitespace, take first line)
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

    /// Log an error to the error log (no more silent failures!)
    pub fn log_error(&mut self, entry: ErrorEntry) {
        // Also show in status bar
        self.status_state.status_message = Some(entry.display());

        // Keep last N errors in log
        self.error_log.push(entry);
        if self.error_log.len() > UI_ERROR_LOG_MAX_SIZE {
            self.error_log.remove(0);
        }
    }

    /// Convenience: log a simple error message
    pub fn log_error_msg(&mut self, severity: ErrorSeverity, msg: impl Into<String>) {
        self.log_error(ErrorEntry::new(severity, msg.into()));
    }

    /// Convenience: log error with context
    pub fn log_error_with_context(
        &mut self,
        severity: ErrorSeverity,
        msg: impl Into<String>,
        context: impl Into<String>,
    ) {
        self.log_error(ErrorEntry::with_context(severity, msg.into(), context.into()));
    }

    /// Get recent errors (last N)
    pub fn recent_errors(&self, count: usize) -> Vec<&ErrorEntry> {
        self.error_log.iter().rev().take(count).collect()
    }

    
    /// Calculate the maximum scroll offset (bottom of content)
    pub fn calculate_max_scroll(&self, viewport_height: u16) -> u16 {
        let mut total_lines = 0u16;

        for msg in &self.session_state.messages {
            // Role line: [You] or [Mermaid]
            total_lines += 1;
            // Content lines (can be many for code blocks)
            total_lines += msg.content.lines().count() as u16;
            // Assistant messages have completion indicator (3 lines)
            if matches!(msg.role, MessageRole::Assistant) {
                total_lines += 3;
            }
            // Empty line between messages
            total_lines += 1;
        }

        // Add lines for current response if generating
        if self.app_state.is_generating() && !self.current_response.is_empty() {
            total_lines += 1; // Role line
            total_lines += self.current_response.lines().count() as u16;
            total_lines += 1; // Typing indicator
        }

        // Max scroll is total lines minus viewport height
        total_lines.saturating_sub(viewport_height)
    }

    /// Auto-scroll to bottom of chat
    pub fn auto_scroll_to_bottom(&mut self, viewport_height: u16) {
        if !self.ui_state.chat_state.is_user_scrolling {
            self.ui_state.chat_state.scroll_offset = self.calculate_max_scroll(viewport_height);
        }
    }

    /// Scroll chat view up
    pub fn scroll_up(&mut self, amount: u16) {
        // Calculate max scroll: total lines minus viewport height
        let viewport_height = UI_DEFAULT_VIEWPORT_HEIGHT; // This should be passed in, but keeping for compatibility
        let max_scroll = self.calculate_max_scroll(viewport_height);

        self.ui_state.chat_state.scroll_offset = self
            .ui_state
            .chat_state
            .scroll_offset
            .saturating_add(amount)
            .min(max_scroll);

        // User is manually scrolling if they're not at the bottom
        let threshold = UI_STATUS_MESSAGE_THRESHOLD; // Allow small margin for rounding
        if self.ui_state.chat_state.scroll_offset < max_scroll.saturating_sub(threshold) {
            self.ui_state.chat_state.is_user_scrolling = true;
        }
    }

    /// Scroll chat view down
    pub fn scroll_down(&mut self, amount: u16) {
        self.ui_state.chat_state.scroll_offset = self.ui_state.chat_state.scroll_offset.saturating_sub(amount);

        // If user scrolls close to bottom, resume auto-scrolling
        let viewport_height = UI_DEFAULT_VIEWPORT_HEIGHT; // Should be passed in
        let max_scroll = self.calculate_max_scroll(viewport_height);
        let threshold = UI_STATUS_MESSAGE_THRESHOLD;
        if self.ui_state.chat_state.scroll_offset >= max_scroll.saturating_sub(threshold) {
            self.ui_state.chat_state.is_user_scrolling = false;
            self.ui_state.chat_state.scroll_offset = max_scroll; // Snap to bottom
        }
    }

    /// Quit the application
    pub fn quit(&mut self) {
        self.running = false;
    }

    /// Cycle to the next operation mode
    pub fn cycle_mode(&mut self) {
        self.operation_state.operation_mode = self.operation_state.operation_mode.cycle();
        self.operation_state.bypass_confirmed = false; // Reset confirmation flag when changing modes
        self.set_status(format!("Mode: {}", self.operation_state.operation_mode.display_name()));
    }

    /// Cycle to the previous operation mode
    pub fn cycle_mode_reverse(&mut self) {
        self.operation_state.operation_mode = self.operation_state.operation_mode.cycle_reverse();
        self.operation_state.bypass_confirmed = false;
        self.set_status(format!("Mode: {}", self.operation_state.operation_mode.display_name()));
    }

    /// Set a specific operation mode
    pub fn set_mode(&mut self, mode: OperationMode) {
        if self.operation_state.operation_mode != mode {
            // If switching away from PlanMode and a plan is awaiting approval, cancel it
            if self.operation_state.operation_mode == OperationMode::PlanMode && self.app_state.is_awaiting_plan_approval() {
                self.cancel_plan();
                self.set_status(format!(
                    "Plan cancelled - switched to {}",
                    mode.display_name()
                ));
            }

            self.operation_state.operation_mode = mode;
            self.operation_state.bypass_confirmed = false;
            self.set_status(format!("Mode: {}", mode.display_name()));
        }
    }

    /// Toggle bypass mode (Ctrl+Y shortcut)
    pub fn toggle_bypass_mode(&mut self) {
        if self.operation_state.operation_mode == OperationMode::BypassAll {
            self.set_mode(OperationMode::Normal);
        } else {
            self.set_mode(OperationMode::BypassAll);
        }
    }

    /// Build message history for sending to the model
    /// Includes only user and assistant messages (not system messages from the UI)
    pub fn build_message_history(&self) -> Vec<ChatMessage> {
        self.session_state.messages
            .iter()
            .filter(|msg| msg.role == MessageRole::User || msg.role == MessageRole::Assistant)
            .cloned()
            .collect()
    }

    /// Build message history with token management
    /// Ensures the conversation doesn't exceed the model's context window
    pub fn build_managed_message_history(
        &self,
        max_context_tokens: usize,
        reserve_tokens: usize,
    ) -> Vec<ChatMessage> {
        use crate::utils::Tokenizer;

        let tokenizer = Tokenizer::new(&self.model_state.model_name);
        let available_tokens = max_context_tokens.saturating_sub(reserve_tokens);

        // Get all relevant messages
        let all_messages: Vec<ChatMessage> = self
            .session_state
            .messages
            .iter()
            .filter(|msg| msg.role == MessageRole::User || msg.role == MessageRole::Assistant)
            .cloned()
            .collect();

        // If no messages, return empty
        if all_messages.is_empty() {
            return Vec::new();
        }

        // Try to keep all messages first
        let messages_for_counting: Vec<(String, String)> = all_messages
            .iter()
            .map(|msg| {
                let role = match msg.role {
                    MessageRole::User => "user",
                    MessageRole::Assistant => "assistant",
                    MessageRole::System => "system",
                };
                (role.to_string(), msg.content.clone())
            })
            .collect();

        let total_tokens = tokenizer
            .count_chat_tokens(&messages_for_counting)
            .unwrap_or_else(|_| {
                // Fallback: estimate 4 chars per token
                all_messages.iter().map(|m| m.content.len() / 4).sum()
            });

        // If we're within budget, return all messages
        if total_tokens <= available_tokens {
            return all_messages;
        }

        // Otherwise, trim from the beginning, keeping the most recent messages
        // Always keep at least the last message pair (user + assistant)
        let mut kept_messages = Vec::new();
        let mut current_tokens = 0;

        // Start from the most recent and work backwards
        for msg in all_messages.iter().rev() {
            let msg_text = vec![(
                match msg.role {
                    MessageRole::User => "user",
                    MessageRole::Assistant => "assistant",
                    MessageRole::System => "system",
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
                // Always keep at least one message pair
                kept_messages.push(msg.clone());
                break;
            } else {
                break;
            }
        }

        // Reverse to restore chronological order
        kept_messages.reverse();
        kept_messages
    }

    /// Load a conversation history
    pub fn load_conversation(&mut self, conversation: ConversationHistory) {
        // Load messages from the conversation
        self.session_state.messages = conversation.messages.clone();
        self.session_state.current_conversation = Some(conversation);
        self.set_status("Conversation loaded");
    }

    /// Save the current conversation
    pub fn save_conversation(&mut self) -> anyhow::Result<()> {
        if let Some(ref manager) = self.session_state.conversation_manager {
            if let Some(ref mut conv) = self.session_state.current_conversation {
                // Update messages in conversation
                conv.messages = self.session_state.messages.clone();
                manager.save_conversation(conv)?;
                self.set_status("Conversation saved");
            }
        }
        Ok(())
    }

    /// Auto-save the conversation (called on exit)
    pub fn auto_save_conversation(&mut self) {
        if self.session_state.messages.is_empty() {
            return; // Don't save empty conversations
        }

        if let Err(e) = self.save_conversation() {
            eprintln!("Failed to auto-save conversation: {}", e);
        }
    }

    // ===== Generation State Transitions =====

    /// Start generation - transitions to Generating state with Sending status
    pub fn start_generation(&mut self, abort_handle: tokio::task::AbortHandle) {
        self.app_state = AppState::Generating {
            status: GenerationStatus::Sending,
            start_time: std::time::Instant::now(),
            tokens_received: 0,
            abort_handle: Some(abort_handle),
        };
    }

    /// Transition from Sending to Thinking status
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

    /// Transition from Thinking/Sending to Streaming status
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

    /// Increment tokens received during generation
    pub fn increment_tokens(&mut self, count: usize) {
        if let AppState::Generating { status, start_time, tokens_received, ref abort_handle } = self.app_state {
            self.app_state = AppState::Generating {
                status,
                start_time,
                tokens_received: tokens_received + count,
                abort_handle: abort_handle.clone(),
            };
            // Also add to cumulative tokens for the entire conversation
            self.session_state.cumulative_tokens += count;
        }
    }

    /// Stop generation - transitions back to Idle state
    pub fn stop_generation(&mut self) {
        self.app_state = AppState::Idle;
    }

    /// Abort generation and return the abort handle if present
    pub fn abort_generation(&mut self) -> Option<tokio::task::AbortHandle> {
        if let AppState::Generating { abort_handle, .. } = &mut self.app_state {
            let handle = abort_handle.take();
            self.app_state = AppState::Idle;
            handle
        } else {
            None
        }
    }

    // ===== Action Confirmation State Transitions =====

    /// Set pending action - transitions to AwaitingActionConfirmation state
    pub fn set_pending_action(&mut self, action: crate::agents::AgentAction, executor: crate::agents::ModeAwareExecutor) {
        self.app_state = AppState::AwaitingActionConfirmation {
            action,
            executor,
        };
    }

    /// Clear pending action - transitions back to Idle state
    pub fn clear_pending_action(&mut self) {
        self.app_state = AppState::Idle;
    }

    // ===== Plan State Transitions =====

    /// Set active plan and enter approval state
    pub fn set_plan(&mut self, plan: Plan) {
        self.app_state = AppState::AwaitingPlanApproval {
            plan: plan.clone(),
            explanation: plan.explanation.clone(),
        };
        self.operation_state.plan_execution_index = None;
        self.set_status("Plan ready - Ctrl+Y to approve, Ctrl+N to cancel");
    }

    /// Cancel the active plan
    pub fn cancel_plan(&mut self) {
        self.app_state = AppState::Idle;
        self.operation_state.plan_execution_index = None;
        self.operation_state.plan_mode_active_for_generation = false;
        self.set_status("Plan cancelled");
    }

    /// Start executing the active plan
    pub fn start_plan_execution(&mut self) {
        if let AppState::AwaitingPlanApproval { plan, .. } = &self.app_state {
            let plan_clone = plan.clone();
            self.app_state = AppState::ExecutingPlan {
                plan: plan_clone,
                current_step: 0,
                start_time: std::time::Instant::now(),
            };
            self.operation_state.plan_execution_index = Some(0);
            self.set_status("Executing plan...");
        }
    }

    /// Get the next pending action from the plan
    pub fn plan_next_action(&self) -> Option<&crate::agents::PlannedAction> {
        self.app_state
            .active_plan()
            .and_then(|plan| plan.next_pending_action().map(|(_, action)| action))
    }

    /// Mark current plan action as completed
    pub fn mark_plan_action_completed(&mut self, result: Option<crate::agents::ActionResult>) {
        if let AppState::ExecutingPlan { plan, current_step, start_time } = &mut self.app_state {
            let mut plan_clone = plan.clone();
            if let Some(index) = self.operation_state.plan_execution_index {
                plan_clone.update_action_status(
                    index,
                    crate::agents::ActionStatus::Completed,
                    result,
                    None,
                );
                self.operation_state.plan_execution_index = Some(index + 1);
                // Update the plan in the state
                self.app_state = AppState::ExecutingPlan {
                    plan: plan_clone,
                    current_step: *current_step + 1,
                    start_time: *start_time,
                };
            }
        }
    }

    /// Mark current plan action as failed
    pub fn mark_plan_action_failed(&mut self, error: String) {
        if let AppState::ExecutingPlan { plan, current_step, start_time } = &mut self.app_state {
            let mut plan_clone = plan.clone();
            if let Some(index) = self.operation_state.plan_execution_index {
                plan_clone.update_action_status(
                    index,
                    crate::agents::ActionStatus::Failed,
                    None,
                    Some(error),
                );
                self.operation_state.plan_execution_index = Some(index + 1);
                // Update the plan in the state
                self.app_state = AppState::ExecutingPlan {
                    plan: plan_clone,
                    current_step: *current_step + 1,
                    start_time: *start_time,
                };
            }
        }
    }

    /// Check if plan execution is complete
    pub fn is_plan_complete(&self) -> bool {
        if let Some(plan) = self.app_state.active_plan() {
            plan.stats().is_complete()
        } else {
            false
        }
    }

    /// Get plan statistics
    pub fn get_plan_stats(&self) -> Option<crate::agents::PlanStats> {
        self.app_state.active_plan().map(|plan| plan.stats())
    }
}

// AppState removed - we're always in "chat" mode now

/// State for action confirmation
#[derive(Debug, Clone)]
pub struct ConfirmationState {
    pub action: AgentAction,
    pub action_description: String,
    pub preview_lines: Vec<String>,  // First few lines for preview
    pub file_info: Option<FileInfo>, // Size, path, overwrite status
    pub allow_always: bool,          // Can user select "always approve"?
}

#[derive(Debug, Clone)]
pub struct FileInfo {
    pub path: String,
    pub size: usize,
    pub exists: bool,
    pub language: Option<String>,
}
