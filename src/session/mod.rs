/// Session management module - Gateway
mod conversation;
pub mod scratchpad;
mod selector;

pub use conversation::{
    ConversationHistory, ConversationManager, ConversationMeta, detect_git_branch, detect_git_sha,
};
pub use selector::{SessionEntry, select_conversation};
