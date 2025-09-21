use super::mode::OperationMode;
use crate::agents::{ModeAwareExecutor, AgentAction};
use crate::diagnostics::{DiagnosticsMode, HardwareMonitor, HardwareStats};
use crate::models::{ChatMessage, MessageRole, Model, ProjectContext};
use crate::session::{ConversationHistory, ConversationManager};
use std::sync::Arc;
use tokio::sync::Mutex;

/// Application state
pub struct App {
    /// Current chat messages
    pub messages: Vec<ChatMessage>,
    /// User input buffer
    pub input: String,
    /// Is the app running?
    pub running: bool,
    /// Current model
    pub model: Arc<Mutex<Box<dyn Model>>>,
    /// Project context
    pub context: ProjectContext,
    /// Current model response (for streaming)
    pub current_response: String,
    /// Is model currently generating?
    pub is_generating: bool,
    /// Scroll offset for chat view
    pub scroll_offset: u16,
    /// Selected message index (for navigation)
    pub selected_message: Option<usize>,
    /// Show file tree sidebar
    pub show_sidebar: bool,
    /// Sidebar expanded to show all files
    pub sidebar_expanded: bool,
    /// Current working directory
    pub working_dir: String,
    /// Model name for display
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
    /// Status text to show during file reading
    pub reading_file_status: Option<String>,
    /// Current confirmation state
    pub confirmation_state: Option<ConfirmationState>,
    /// Track if user is manually scrolling (not at bottom)
    pub is_user_scrolling: bool,
    /// Track last time status was set for timeout
    pub status_timestamp: Option<std::time::Instant>,
    /// Abort handle for canceling generation
    pub generation_abort: Option<tokio::task::AbortHandle>,
    /// Conversation manager for persistence
    pub conversation_manager: Option<ConversationManager>,
    /// Current conversation being tracked
    pub current_conversation: Option<ConversationHistory>,
    /// Hardware monitor
    pub hardware_monitor: Option<Arc<Mutex<HardwareMonitor>>>,
    /// Current hardware stats
    pub hardware_stats: Option<HardwareStats>,
    /// Diagnostics display mode
    pub diagnostics_mode: DiagnosticsMode,
}

impl App {
    /// Create a new app instance
    pub fn new(model: Box<dyn Model>, context: ProjectContext) -> Self {
        let model_name = model.name().to_string();
        let working_dir = std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| ".".to_string());

        // Initialize conversation manager for the current directory
        let conversation_manager = ConversationManager::new(&working_dir).ok();
        let current_conversation = conversation_manager.as_ref().map(|_| {
            ConversationHistory::new(working_dir.clone(), model_name.clone())
        });

        // Initialize hardware monitor
        let hardware_monitor = Some(Arc::new(Mutex::new(HardwareMonitor::new())));

        Self {
            messages: Vec::new(),
            input: String::new(),
            running: true,
            model: Arc::new(Mutex::new(model)),
            context,
            current_response: String::new(),
            is_generating: false,
            scroll_offset: 0,
            selected_message: None,
            show_sidebar: true,
            sidebar_expanded: false,
            working_dir,
            model_name,
            status_message: None,
            operation_mode: OperationMode::default(), // Starts in Normal mode
            bypass_confirmed: false,
            pending_action: None,
            pending_executor: None,
            pending_file_read: false,
            reading_file_status: None,
            confirmation_state: None,
            is_user_scrolling: false,
            status_timestamp: None,
            generation_abort: None,
            conversation_manager,
            current_conversation,
            hardware_monitor,
            hardware_stats: None,
            diagnostics_mode: DiagnosticsMode::Compact,
        }
    }

    /// Add a message to the chat
    pub fn add_message(&mut self, role: MessageRole, content: String) {
        let message = ChatMessage {
            role,
            content,
            timestamp: chrono::Local::now(),
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
        if !self.is_user_scrolling {
            self.scroll_offset = self.calculate_max_scroll(viewport_height);
        }
    }

    /// Scroll chat view up
    pub fn scroll_up(&mut self, amount: u16) {
        // Calculate max scroll: total lines minus viewport height
        let viewport_height = 20;  // This should be passed in, but keeping for compatibility
        let max_scroll = self.calculate_max_scroll(viewport_height);

        self.scroll_offset = self.scroll_offset
            .saturating_add(amount)
            .min(max_scroll);

        // User is manually scrolling if they're not at the bottom
        let threshold = 3;  // Allow small margin for rounding
        if self.scroll_offset < max_scroll.saturating_sub(threshold) {
            self.is_user_scrolling = true;
        }
    }

    /// Scroll chat view down
    pub fn scroll_down(&mut self, amount: u16) {
        self.scroll_offset = self.scroll_offset.saturating_sub(amount);

        // If user scrolls close to bottom, resume auto-scrolling
        let viewport_height = 20;  // Should be passed in
        let max_scroll = self.calculate_max_scroll(viewport_height);
        let threshold = 3;
        if self.scroll_offset >= max_scroll.saturating_sub(threshold) {
            self.is_user_scrolling = false;
            self.scroll_offset = max_scroll;  // Snap to bottom
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
    pub fn build_managed_message_history(&self, max_context_tokens: usize, reserve_tokens: usize) -> Vec<ChatMessage> {
        use crate::utils::Tokenizer;

        let tokenizer = Tokenizer::new(&self.model_name);
        let available_tokens = max_context_tokens.saturating_sub(reserve_tokens);

        // Get all relevant messages
        let all_messages: Vec<ChatMessage> = self.messages
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

        let total_tokens = tokenizer.count_chat_tokens(&messages_for_counting)
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
                }.to_string(),
                msg.content.clone()
            )];

            let msg_tokens = tokenizer.count_chat_tokens(&msg_text)
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
}

// AppState removed - we're always in "chat" mode now

/// State for action confirmation
#[derive(Debug, Clone)]
pub struct ConfirmationState {
    pub action: AgentAction,
    pub action_description: String,
    pub preview_lines: Vec<String>,  // First few lines for preview
    pub file_info: Option<FileInfo>, // Size, path, overwrite status
    pub allow_always: bool,           // Can user select "always approve"?
}

#[derive(Debug, Clone)]
pub struct FileInfo {
    pub path: String,
    pub size: usize,
    pub exists: bool,
    pub language: Option<String>,
}