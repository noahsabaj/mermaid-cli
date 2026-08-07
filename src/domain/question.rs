//! Re-export shim: the question value types moved to
//! [`mermaid_core::question`].
//!
//! `ToolMetadata::Questions` carries `Vec<QuestionAnswer>`, and that enum is
//! embedded in `ChatMessage` through `ActionDisplay`, so these types are part
//! of the wire/persistence surface rather than reducer-only state.

pub use mermaid_core::question::*;
