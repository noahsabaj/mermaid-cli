/// Session management module - Gateway
mod conversation;
pub mod scratchpad;
mod selector;

pub use conversation::{
    ConversationManager, ConversationMeta, detect_git_branch, detect_git_sha,
    probe_session_provenance,
};
pub use selector::{SessionEntry, select_conversation};
