/// Session management module - Gateway
mod conversation;
mod selector;

pub use conversation::{ConversationHistory, ConversationManager, detect_git_branch};
pub use selector::{SessionEntry, select_conversation};
