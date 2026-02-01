/// Session management module - Gateway
mod conversation;
mod selector;

pub use conversation::{ConversationHistory, ConversationManager};
pub use selector::select_conversation;
