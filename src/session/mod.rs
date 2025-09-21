/// Session management module - Gateway

mod conversation;
mod selector;
mod state;

pub use conversation::{ConversationHistory, ConversationManager};
pub use selector::select_conversation;
pub use state::SessionState;