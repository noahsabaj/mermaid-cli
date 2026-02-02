/// Operation state management
///
/// Minimal state for tracking active operations.

use std::collections::VecDeque;

/// Operation state - tracks file reading, tool calls, and queued messages
pub struct OperationState {
    /// Track if FILE_READ feedback is pending
    pub pending_file_read: bool,
    /// Status text to show during file reading
    pub reading_file_status: Option<String>,
    /// Accumulated tool calls during streaming (persists across process_stream_chunks calls)
    pub accumulated_tool_calls: Vec<crate::models::ToolCall>,
    /// Queued messages - typed while model is generating, will be sent in order
    pub queued_messages: VecDeque<String>,
}

impl OperationState {
    /// Create a new OperationState with default values
    pub fn new() -> Self {
        Self {
            pending_file_read: false,
            reading_file_status: None,
            accumulated_tool_calls: Vec::new(),
            queued_messages: VecDeque::new(),
        }
    }

    /// Queue a message to be sent after current generation
    pub fn queue_message(&mut self, message: String) {
        self.queued_messages.push_back(message);
    }

    /// Take the next queued message (removes from front of queue)
    pub fn take_queued_message(&mut self) -> Option<String> {
        self.queued_messages.pop_front()
    }

    /// Check if there are any queued messages
    pub fn has_queued_message(&self) -> bool {
        !self.queued_messages.is_empty()
    }

    /// Get all queued messages for display (doesn't remove them)
    pub fn get_queued_messages(&self) -> &VecDeque<String> {
        &self.queued_messages
    }

    /// Get the count of queued messages
    pub fn queued_message_count(&self) -> usize {
        self.queued_messages.len()
    }
}

impl Default for OperationState {
    fn default() -> Self {
        Self::new()
    }
}
