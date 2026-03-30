//! Operation state management
//!
//! Minimal state for tracking active operations.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::agents::SubagentProgress;
use crate::utils::MutexExt;

/// Operation state - tracks tool calls and queued messages
pub struct OperationState {
    /// Accumulated tool calls during streaming (persists across process_stream_chunks calls)
    pub accumulated_tool_calls: Vec<crate::models::ToolCall>,
    /// Queued messages - typed while model is generating, will be sent in order
    pub queued_messages: VecDeque<String>,
    /// Shared progress state for active subagents (None when no agents running)
    pub active_subagents: Option<Arc<Mutex<Vec<SubagentProgress>>>>,
}

impl OperationState {
    /// Create a new OperationState with default values
    pub fn new() -> Self {
        Self {
            accumulated_tool_calls: Vec::new(),
            queued_messages: VecDeque::new(),
            active_subagents: None,
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

    /// Snapshot the active subagent progress for rendering.
    /// Locks the mutex briefly and clones the vec (max 10 small entries).
    pub fn snapshot_subagent_progress(&self) -> Option<Vec<SubagentProgress>> {
        self.active_subagents
            .as_ref()
            .map(|p| p.lock_mut_safe().clone())
    }

    /// Clear the active subagents state
    pub fn clear_subagents(&mut self) {
        self.active_subagents = None;
    }
}

impl Default for OperationState {
    fn default() -> Self {
        Self::new()
    }
}
