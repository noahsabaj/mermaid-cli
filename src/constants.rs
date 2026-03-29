//! Constants module to avoid magic numbers in the codebase

// Network Configuration
pub const DEFAULT_OLLAMA_PORT: u16 = 11434;

// Timeouts
pub const COMMAND_TIMEOUT_SECS: u64 = 30;
pub const COMMAND_MAX_TIMEOUT_SECS: u64 = 300;

// UI Configuration
pub const UI_POLL_INTERVAL_MS: u64 = 50;
pub const UI_MOUSE_SCROLL_LINES: u16 = 3;
pub const UI_ERROR_LOG_MAX_SIZE: usize = 50;

// Default Model Configuration
pub const DEFAULT_TEMPERATURE: f32 = 0.7;
pub const DEFAULT_MAX_TOKENS: usize = 4096;

// Context Management
/// Maximum context tokens for managed message history
pub const MAX_CONTEXT_TOKENS: usize = 75_000;
/// Tokens reserved for the model's response within the context window
pub const CONTEXT_RESERVE_TOKENS: usize = 4_000;
