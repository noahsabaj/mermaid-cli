/// State management modules for the TUI
///
/// Splits the App god object into focused, composable state modules:
/// - generation: State machine for model interactions
/// - error: Structured error logging
/// - conversation: Chat messages and history
/// - model: LLM configuration
/// - ui: Visual presentation
/// - operation: Modes and confirmations
/// - status: Status messages
/// - input: User input buffer

pub mod attachment;
mod conversation;
mod error;
mod generation;
mod input;
mod model;
mod operation;
mod status;
mod ui;

// Re-export all types for easy access
pub use attachment::AttachmentState;
pub use conversation::ConversationState;
pub use error::{ErrorEntry, ErrorSeverity};
pub use generation::{AppState, GenerationStatus};
pub use input::InputBuffer;
pub use model::ModelState;
pub use operation::OperationState;
pub use status::StatusState;
pub use ui::UIState;
