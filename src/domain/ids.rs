//! Re-export shim: the identity newtypes moved to [`mermaid_core::ids`].
//!
//! They are pure value types with no dependencies, and `question::QuestionAnswer`
//! — which `tool_run` embeds — is keyed by them, so they had to sit below the
//! model layer rather than beside the reducer that mints them.

pub use mermaid_core::ids::*;
