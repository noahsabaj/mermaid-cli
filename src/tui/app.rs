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
        // Calculate approximate max scroll based on message count
        // Each message takes roughly 3-4 lines (role + content + spacing)
        let estimated_lines = self.messages.len().saturating_mul(4) as u16;
        let max_scroll = estimated_lines.saturating_sub(10); // Leave some buffer

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