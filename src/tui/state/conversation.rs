/// Conversation state management
///
/// Handles chat messages, history, and persistence.

use std::collections::VecDeque;

use ratatui::text::Line;
use rustc_hash::FxHashMap;

use crate::models::ChatMessage;
use crate::session::{ConversationHistory, ConversationManager};

/// Session state - conversation history and persistence
pub struct ConversationState {
    /// Current chat messages
    pub messages: Vec<ChatMessage>,
    /// Conversation manager for persistence
    pub conversation_manager: Option<ConversationManager>,
    /// Current conversation being tracked
    pub current_conversation: Option<ConversationHistory>,
    /// Input history for arrow key navigation (loaded from session)
    pub input_history: VecDeque<String>,
    /// Current position in history (None = editing current input, Some(i) = viewing history[i])
    pub history_index: Option<usize>,
    /// Saved input when navigating away from current draft
    pub history_buffer: String,
    /// Cumulative token count for the entire conversation
    pub cumulative_tokens: usize,
    /// Auto-generated conversation title (like Claude Code)
    pub conversation_title: Option<String>,
    /// Cached parsed markdown per message: (message_index, content_len) -> parsed lines
    /// Invalidated when content length changes (cheap proxy for content change)
    pub markdown_cache: FxHashMap<(usize, usize), Vec<Line<'static>>>,
}

impl ConversationState {
    /// Create a new ConversationState with default values
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
            conversation_manager: None,
            current_conversation: None,
            input_history: VecDeque::new(),
            history_index: None,
            history_buffer: String::new(),
            cumulative_tokens: 0,
            conversation_title: None,
            markdown_cache: FxHashMap::default(),
        }
    }

    /// Create ConversationState with conversation management
    pub fn with_conversation(
        conversation_manager: Option<ConversationManager>,
        current_conversation: Option<ConversationHistory>,
        input_history: VecDeque<String>,
    ) -> Self {
        Self {
            messages: Vec::new(),
            conversation_manager,
            current_conversation,
            input_history,
            history_index: None,
            history_buffer: String::new(),
            cumulative_tokens: 0,
            conversation_title: None,
            markdown_cache: FxHashMap::default(),
        }
    }

    /// Add tokens to the cumulative count
    pub fn add_tokens(&mut self, count: usize) {
        self.cumulative_tokens += count;
    }

    /// Get message count
    pub fn message_count(&self) -> usize {
        self.messages.len()
    }

    /// Check if conversation is empty
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }
}

impl Default for ConversationState {
    fn default() -> Self {
        Self::new()
    }
}
