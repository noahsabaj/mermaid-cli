use super::mode::OperationMode;
use super::theme::Theme;
use super::widgets::{ChatState, InputState, SidebarState};
use crate::agents::{AgentAction, ModeAwareExecutor, Plan};
use crate::diagnostics::{DiagnosticsMode, HardwareMonitor, HardwareStats};
use crate::models::{ChatMessage, MessageRole, Model, ProjectContext};
use crate::session::{ConversationHistory, ConversationManager};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;

/// Generation status for the status line
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenerationStatus {
    /// Not currently generating
    Idle,
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
            GenerationStatus::Initializing => "Initializing",
            GenerationStatus::Thinking => "Thinking",
            GenerationStatus::Streaming => "Streaming",
        }
    }
}

/// Application state
pub struct App {
    /// Current chat messages
    pub messages: Vec<ChatMessage>,
    /// User input buffer
    pub input: String,
    /// Cursor position in the input string
    pub cursor_position: usize,
    /// Is the app running?
    pub running: bool,
    /// Current model (RwLock for concurrent reads during UI rendering)
    pub model: Arc<RwLock<Box<dyn Model>>>,
    /// Project context
    pub context: ProjectContext,
    /// Current model response (for streaming)
    pub current_response: String,
    /// Is model currently generating?
    pub is_generating: bool,

    // Widget States
    /// Chat widget state (scroll, scrolling flag)
    pub chat_state: ChatState,
    /// Input widget state (cursor position for display)
    pub input_state: InputState,
    /// Sidebar widget state (table state, selection)
    pub sidebar_state: SidebarState,

    /// Selected message index (for navigation)
    pub selected_message: Option<usize>,
    /// Show file tree sidebar
    pub show_sidebar: bool,
    /// Sidebar expanded to show all files
    pub sidebar_expanded: bool,
    /// Current working directory
    pub working_dir: String,
    /// Full model ID (e.g., "ollama/qwen3-coder:30b")
    pub model_id: String,
    /// Model name for display (short version)
    pub model_name: String,
    /// Status message
    pub status_message: Option<String>,
    /// Current operation mode (Normal, AcceptEdits, PlanMode, BypassAll)
    pub operation_mode: OperationMode,
    /// Flag for confirming destructive operations in BypassAll mode
    pub bypass_confirmed: bool,
    /// Pending action waiting for confirmation
    pub pending_action: Option<AgentAction>,
    /// Executor for pending action
    pub pending_executor: Option<ModeAwareExecutor>,
    /// Track if FILE_READ feedback is pending
    pub pending_file_read: bool,
    /// Active plan awaiting approval or being executed
    pub active_plan: Option<Plan>,
    /// Current step index during plan execution (0-based)
    pub plan_execution_index: Option<usize>,
    /// Whether we're waiting for user to approve plan
    pub awaiting_plan_approval: bool,
    /// Track if plan mode was active when generation started
    pub plan_mode_active_for_generation: bool,
    /// Status text to show during file reading
    pub reading_file_status: Option<String>,
    /// Current confirmation state
    pub confirmation_state: Option<ConfirmationState>,
    /// Track last time status was set for timeout
    pub status_timestamp: Option<std::time::Instant>,
    /// Abort handle for canceling generation
    pub generation_abort: Option<tokio::task::AbortHandle>,
    /// Conversation manager for persistence
    pub conversation_manager: Option<ConversationManager>,
    /// Current conversation being tracked
    pub current_conversation: Option<ConversationHistory>,
    /// Hardware monitor (RwLock for concurrent reads every frame)
    pub hardware_monitor: Option<Arc<RwLock<HardwareMonitor>>>,
    /// Current hardware stats
    pub hardware_stats: Option<HardwareStats>,
    /// Diagnostics display mode
    pub diagnostics_mode: DiagnosticsMode,
    /// UI theme
    pub theme: Theme,
    /// Current generation status (Idle, Initializing, Thinking, Streaming)
    pub generation_status: GenerationStatus,
    /// When generation started (for calculating elapsed time)
    pub generation_start_time: Option<Instant>,
    /// Count of tokens received so far during streaming
    pub tokens_received: usize,
    /// Custom status from LLM (e.g., "Analyzing", "Planning") - overrides generation_status text
    pub custom_status: Option<String>,
    /// Input history for arrow key navigation (loaded from session)
    pub input_history: Vec<String>,
    /// Current position in history (None = editing current input, Some(i) = viewing history[i])
    pub history_index: Option<usize>,
    /// Saved input when navigating away from current draft
    pub history_buffer: String,
}

impl App {
    /// Create a new app instance
    pub fn new(model: Box<dyn Model>, context: ProjectContext, model_id: String) -> Self {
        let model_name = model.name().to_string();
        let working_dir = std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| ".".to_string());

        // Initialize conversation manager for the current directory
        let conversation_manager = ConversationManager::new(&working_dir).ok();
        let current_conversation = conversation_manager
            .as_ref()
            .map(|_| ConversationHistory::new(working_dir.clone(), model_name.clone()));

        // Initialize hardware monitor
        let hardware_monitor = Some(Arc::new(RwLock::new(HardwareMonitor::new())));

        // Load input history from conversation if available
        let input_history = conversation_manager
            .as_ref()
            .and_then(|_| current_conversation.as_ref())
            .map(|conv| conv.input_history.clone())
            .unwrap_or_default();

        Self {
            messages: Vec::new(),
            input: String::new(),
            cursor_position: 0,
            running: true,
            model: Arc::new(RwLock::new(model)),
            context,
            current_response: String::new(),
            is_generating: false,
            chat_state: ChatState::new(),
            input_state: InputState::new(),
            sidebar_state: SidebarState::new(),
            selected_message: None,
            show_sidebar: false, // Hidden by default - press Ctrl+S to show
            sidebar_expanded: false,
            working_dir,
            model_id,
            model_name,
            status_message: None,
            operation_mode: OperationMode::default(), // Starts in Normal mode
            bypass_confirmed: false,
            pending_action: None,
            pending_executor: None,
            pending_file_read: false,
            active_plan: None,
            plan_execution_index: None,
            awaiting_plan_approval: false,
            plan_mode_active_for_generation: false,
            reading_file_status: None,
            confirmation_state: None,
            status_timestamp: None,
            generation_abort: None,
            conversation_manager,
            current_conversation,
            hardware_monitor,
            hardware_stats: None,
            diagnostics_mode: DiagnosticsMode::Compact,
            theme: Theme::dark(), // Default to dark theme
            generation_status: GenerationStatus::Idle,
            generation_start_time: None,
            tokens_received: 0,
            custom_status: None,
            input_history,
            history_index: None,
            history_buffer: String::new(),
        }
    }

    /// Add a message to the chat
    pub fn add_message(&mut self, role: MessageRole, content: String) {
        let message = ChatMessage {
            role,
            content,
            timestamp: chrono::Local::now(),
            actions: Vec::new(),
        };
        self.messages.push(message.clone());

        // Update current conversation
        if let Some(ref mut conv) = self.current_conversation {
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

    /// Toggle sidebar visibility
    pub fn toggle_sidebar(&mut self) {
        self.show_sidebar = !self.show_sidebar;
    }

    /// Set status message
    pub fn set_status(&mut self, message: impl Into<String>) {
        self.status_message = Some(message.into());
    }

    /// Clear status message
    pub fn clear_status(&mut self) {
        self.status_message = None;
    }

    /// Calculate the maximum scroll offset (bottom of content)
    pub fn calculate_max_scroll(&self, viewport_height: u16) -> u16 {
        let mut total_lines = 0u16;

        for msg in &self.messages {
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
        if self.is_generating && !self.current_response.is_empty() {
            total_lines += 1; // Role line
            total_lines += self.current_response.lines().count() as u16;
            total_lines += 1; // Typing indicator
        }

        // Max scroll is total lines minus viewport height
        total_lines.saturating_sub(viewport_height)
    }

    /// Auto-scroll to bottom of chat
    pub fn auto_scroll_to_bottom(&mut self, viewport_height: u16) {
        if !self.chat_state.is_user_scrolling {
            self.chat_state.scroll_offset = self.calculate_max_scroll(viewport_height);
        }
    }

    /// Scroll chat view up
    pub fn scroll_up(&mut self, amount: u16) {
        // Calculate max scroll: total lines minus viewport height
        let viewport_height = 20; // This should be passed in, but keeping for compatibility
        let max_scroll = self.calculate_max_scroll(viewport_height);

        self.chat_state.scroll_offset = self
            .chat_state
            .scroll_offset
            .saturating_add(amount)
            .min(max_scroll);

        // User is manually scrolling if they're not at the bottom
        let threshold = 3; // Allow small margin for rounding
        if self.chat_state.scroll_offset < max_scroll.saturating_sub(threshold) {
            self.chat_state.is_user_scrolling = true;
        }
    }

    /// Scroll chat view down
    pub fn scroll_down(&mut self, amount: u16) {
        self.chat_state.scroll_offset = self.chat_state.scroll_offset.saturating_sub(amount);

        // If user scrolls close to bottom, resume auto-scrolling
        let viewport_height = 20; // Should be passed in
        let max_scroll = self.calculate_max_scroll(viewport_height);
        let threshold = 3;
        if self.chat_state.scroll_offset >= max_scroll.saturating_sub(threshold) {
            self.chat_state.is_user_scrolling = false;
            self.chat_state.scroll_offset = max_scroll; // Snap to bottom
        }
    }

    /// Quit the application
    pub fn quit(&mut self) {
        self.running = false;
    }

    /// Cycle to the next operation mode
    pub fn cycle_mode(&mut self) {
        self.operation_mode = self.operation_mode.cycle();
        self.bypass_confirmed = false; // Reset confirmation flag when changing modes
        self.set_status(format!("Mode: {}", self.operation_mode.display_name()));
    }

    /// Cycle to the previous operation mode
    pub fn cycle_mode_reverse(&mut self) {
        self.operation_mode = self.operation_mode.cycle_reverse();
        self.bypass_confirmed = false;
        self.set_status(format!("Mode: {}", self.operation_mode.display_name()));
    }

    /// Set a specific operation mode
    pub fn set_mode(&mut self, mode: OperationMode) {
        if self.operation_mode != mode {
            // If switching away from PlanMode and a plan is awaiting approval, cancel it
            if self.operation_mode == OperationMode::PlanMode && self.awaiting_plan_approval {
                self.cancel_plan();
                self.set_status(format!(
                    "Plan cancelled - switched to {}",
                    mode.display_name()
                ));
            }

            self.operation_mode = mode;
            self.bypass_confirmed = false;
            self.set_status(format!("Mode: {}", mode.display_name()));
        }
    }

    /// Toggle bypass mode (Ctrl+Y shortcut)
    pub fn toggle_bypass_mode(&mut self) {
        if self.operation_mode == OperationMode::BypassAll {
            self.set_mode(OperationMode::Normal);
        } else {
            self.set_mode(OperationMode::BypassAll);
        }
    }

    /// Build message history for sending to the model
    /// Includes only user and assistant messages (not system messages from the UI)
    pub fn build_message_history(&self) -> Vec<ChatMessage> {
        self.messages
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

        let tokenizer = Tokenizer::new(&self.model_name);
        let available_tokens = max_context_tokens.saturating_sub(reserve_tokens);

        // Get all relevant messages
        let all_messages: Vec<ChatMessage> = self
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
        self.messages = conversation.messages.clone();
        self.current_conversation = Some(conversation);
        self.set_status("Conversation loaded");
    }

    /// Save the current conversation
    pub fn save_conversation(&mut self) -> anyhow::Result<()> {
        if let Some(ref manager) = self.conversation_manager {
            if let Some(ref mut conv) = self.current_conversation {
                // Update messages in conversation
                conv.messages = self.messages.clone();
                manager.save_conversation(conv)?;
                self.set_status("Conversation saved");
            }
        }
        Ok(())
    }

    /// Auto-save the conversation (called on exit)
    pub fn auto_save_conversation(&mut self) {
        if self.messages.is_empty() {
            return; // Don't save empty conversations
        }

        if let Err(e) = self.save_conversation() {
            eprintln!("Failed to auto-save conversation: {}", e);
        }
    }

    /// Toggle diagnostics display mode
    pub fn toggle_diagnostics(&mut self) {
        self.diagnostics_mode = match self.diagnostics_mode {
            DiagnosticsMode::Hidden => DiagnosticsMode::Compact,
            DiagnosticsMode::Compact => DiagnosticsMode::Detailed,
            DiagnosticsMode::Detailed => DiagnosticsMode::Hidden,
        };

        self.set_status(format!("Diagnostics: {:?}", self.diagnostics_mode));
    }

    /// Update hardware stats
    pub fn update_hardware_stats(&mut self, stats: HardwareStats) {
        self.hardware_stats = Some(stats);
    }

    /// Set active plan and enter approval state
    pub fn set_plan(&mut self, plan: Plan) {
        self.active_plan = Some(plan);
        self.awaiting_plan_approval = true;
        self.plan_execution_index = None;
        self.set_status("Plan ready - Alt+Y to approve, Alt+N to cancel");
    }

    /// Cancel the active plan
    pub fn cancel_plan(&mut self) {
        self.active_plan = None;
        self.awaiting_plan_approval = false;
        self.plan_execution_index = None;
        self.plan_mode_active_for_generation = false;
        self.set_status("Plan cancelled");
    }

    /// Start executing the active plan
    pub fn start_plan_execution(&mut self) {
        if self.active_plan.is_some() {
            self.plan_execution_index = Some(0);
            self.awaiting_plan_approval = false;
            self.set_status("Executing plan...");
        }
    }

    /// Get the next pending action from the plan
    pub fn plan_next_action(&self) -> Option<&crate::agents::PlannedAction> {
        self.active_plan
            .as_ref()
            .and_then(|plan| plan.next_pending_action().map(|(_, action)| action))
    }

    /// Mark current plan action as completed
    pub fn mark_plan_action_completed(&mut self, result: Option<crate::agents::ActionResult>) {
        if let Some(plan) = self.active_plan.as_mut() {
            if let Some(index) = self.plan_execution_index {
                plan.update_action_status(
                    index,
                    crate::agents::ActionStatus::Completed,
                    result,
                    None,
                );
                self.plan_execution_index = Some(index + 1);
            }
        }
    }

    /// Mark current plan action as failed
    pub fn mark_plan_action_failed(&mut self, error: String) {
        if let Some(plan) = self.active_plan.as_mut() {
            if let Some(index) = self.plan_execution_index {
                plan.update_action_status(
                    index,
                    crate::agents::ActionStatus::Failed,
                    None,
                    Some(error),
                );
                self.plan_execution_index = Some(index + 1);
            }
        }
    }

    /// Check if plan execution is complete
    pub fn is_plan_complete(&self) -> bool {
        if let Some(plan) = self.active_plan.as_ref() {
            plan.stats().is_complete()
        } else {
            false
        }
    }

    /// Get plan statistics
    pub fn get_plan_stats(&self) -> Option<crate::agents::PlanStats> {
        self.active_plan.as_ref().map(|plan| plan.stats())
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
