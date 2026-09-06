/// Session management module - Gateway
mod conversation;
mod event_log;
mod index;
pub mod scratchpad;
mod selector;

pub use conversation::{
    ConversationManager, ConversationMeta, detect_git_branch, detect_git_sha,
    probe_session_provenance,
};
pub use index::{SessionIndexReport, rebuild_session_index, session_row};
pub use selector::{SessionEntry, select_conversation};
