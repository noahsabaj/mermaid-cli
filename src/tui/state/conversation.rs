//! Conversation state management
//!
//! Handles chat messages, persistence, and conversation metadata.

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
    /// Cumulative token count for the entire conversation
    pub cumulative_tokens: usize,
    /// Auto-generated conversation title (like Claude Code)
    pub conversation_title: Option<String>,
}

impl ConversationState {
    /// Create a new ConversationState with default values
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
            conversation_manager: None,
            current_conversation: None,
            cumulative_tokens: 0,
            conversation_title: None,
        }
    }

    /// Create ConversationState with conversation management
    pub fn with_conversation(
        conversation_manager: Option<ConversationManager>,
        current_conversation: Option<ConversationHistory>,
    ) -> Self {
        Self {
            messages: Vec::new(),
            conversation_manager,
            current_conversation,
            cumulative_tokens: 0,
            conversation_title: None,
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
