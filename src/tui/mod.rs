// Gateway module for TUI - follows the Train Station Pattern
// All external access must go through this gateway

// Private submodules - not directly accessible from outside
mod action_handler;
mod app;
mod command_handler;
mod event_handler;
mod loop_coordinator;
mod markdown;
mod render;
pub mod state;
mod stream_handler;
mod theme;
mod ui;
mod widgets;

// Public re-exports - the ONLY way to access TUI functionality
pub use app::App;
pub use state::{
    AppState, ConversationState, ErrorEntry, ErrorSeverity,
    GenerationStatus, InputBuffer, ModelState, OperationState, StatusState, UIState,
};
pub use ui::run_ui;
