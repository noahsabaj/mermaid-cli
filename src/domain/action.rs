//! Re-export shim: the action display value types moved to
//! [`mermaid_core::action`].
//!
//! They had to: `models::ChatMessage` embeds `Vec<ActionDisplay>`, so while
//! these lived in `domain` the model layer reached up into the MVU layer for
//! its own field types — the last cycle blocking `mermaid-core`. The shim keeps
//! `domain::ActionDisplay` and `domain::action::*` resolving unchanged.

pub use mermaid_core::action::{ActionDetails, ActionDisplay, ActionResult};
