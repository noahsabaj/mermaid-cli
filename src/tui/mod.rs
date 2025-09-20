// Gateway module for TUI - follows the Train Station Pattern
// All external access must go through this gateway

// Private submodules - not directly accessible from outside
mod app;
mod chat;
mod input;
mod render;
mod ui;

// Public re-exports - the ONLY way to access TUI functionality
pub use app::{App, AppState};
pub use ui::run_ui;