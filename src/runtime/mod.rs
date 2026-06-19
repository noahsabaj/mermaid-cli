//! Compatibility re-export for the internal runtime crate.
//!
//! New daemon, CLI, and TUI code should depend on the
//! `mermaid-runtime` crate boundary. The `mermaid_cli::runtime` module is
//! retained so existing call sites and downstream code do not need a
//! flag-day import rewrite.

pub mod client;

pub use client::*;
pub use mermaid_runtime::*;
