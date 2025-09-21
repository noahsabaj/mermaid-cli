use super::mode::OperationMode;
use crate::agents::{ModeAwareExecutor, AgentAction};
use crate::models::{Model, ProjectContext};
use std::sync::Arc;
use tokio::sync::Mutex;

/// Represents a chat message
#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub role: MessageRole,
    pub content: String,
    pub timestamp: chrono::DateTime<chrono::Local>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MessageRole {
    User,
    Assistant,
    System,
}

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
}

impl App {
    /// Create a new app instance
    pub fn new(model: Box<dyn Model>, context: ProjectContext) -> Self {
        let model_name = model.name().to_string();
        let working_dir = std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| ".".to_string());

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
        }
    }

    /// Add a message to the chat
    pub fn add_message(&mut self, role: MessageRole, content: String) {
        self.messages.push(ChatMessage {
            role,
            content,
            timestamp: chrono::Local::now(),
        });

        // Auto-scroll to bottom
        self.scroll_offset = 0;
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

    /// Scroll chat view up
    pub fn scroll_up(&mut self, amount: u16) {
        // Count actual lines that will be rendered
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

        // Calculate max scroll: total lines minus viewport height
        // Use 20 as a safe estimate for viewport height
        let viewport_height = 20;
        let max_scroll = total_lines.saturating_sub(viewport_height);

        self.scroll_offset = self.scroll_offset
            .saturating_add(amount)
            .min(max_scroll);
    }

    /// Scroll chat view down
    pub fn scroll_down(&mut self, amount: u16) {
        self.scroll_offset = self.scroll_offset.saturating_sub(amount);
    }

    /// Quit the application
    pub fn quit(&mut self) {
        self.running = false;
    }

    /// Cycle to the next operation mode
    pub fn cycle_mode(&mut self) {
        self.operation_mode = self.operation_mode.cycle();
        self.bypass_confirmed = false; // Reset confirmation flag when changing modes
        self.set_status(format!("Operation mode: {} - {}",
            self.operation_mode.display_name(),
            self.operation_mode.description()
        ));
    }

    /// Cycle to the previous operation mode
    pub fn cycle_mode_reverse(&mut self) {
        self.operation_mode = self.operation_mode.cycle_reverse();
        self.bypass_confirmed = false;
        self.set_status(format!("Operation mode: {} - {}",
            self.operation_mode.display_name(),
            self.operation_mode.description()
        ));
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
}

/// Application state for different modes
#[derive(Debug, Clone, PartialEq)]
pub enum AppState {
    /// Normal mode - viewing chat
    Normal,
    /// Insert mode - typing input
    Insert,
    /// Command mode - entering commands
    Command,
    /// Selecting files
    FileSelect,
}