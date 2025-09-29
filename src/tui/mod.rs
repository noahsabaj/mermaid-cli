// Gateway module for TUI - follows the Train Station Pattern
// All external access must go through this gateway

// Private submodules - not directly accessible from outside
mod app;
mod markdown;
mod mode;
mod render;
mod ui;
mod event_handler;
mod command_handler;
mod action_handler;
mod stream_handler;
mod loop_coordinator;

// Public re-exports - the ONLY way to access TUI functionality
pub use app::{App, ConfirmationState, FileInfo};
pub use mode::OperationMode;
pub use ui::run_ui;
