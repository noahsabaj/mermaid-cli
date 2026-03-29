//! Generation state machine
//!
//! Tracks the application lifecycle during model interactions.

use std::time::Instant;

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
}
