//! Status state management
//!
//! UI status messages and timing.

use std::time::Instant;

/// Status state - UI status messages and timing
#[derive(Debug)]
pub struct StatusState {
    pub status_message: Option<String>,
    pub status_timestamp: Option<Instant>,
}

impl StatusState {
    pub fn new() -> Self {
        Self {
            status_message: None,
            status_timestamp: None,
        }
    }

    /// Set a status message
    pub fn set(&mut self, message: impl Into<String>) {
        self.status_message = Some(message.into());
        self.status_timestamp = Some(Instant::now());
    }

    /// Clear the status message
    pub fn clear(&mut self) {
        self.status_message = None;
        self.status_timestamp = None;
    }
}

impl Default for StatusState {
    fn default() -> Self {
        Self::new()
    }
}
