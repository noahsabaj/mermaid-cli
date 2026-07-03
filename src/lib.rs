// `collapsible_match` fires on several pre-existing `match` arms across
// render/session/reducer modules (e.g. `Pat => { if cond { .. } }`). These are
// purely stylistic and unrelated to this change; suppressed crate-wide so CI's
// `clippy -D warnings` stays green. Safe to refactor away later.
#![allow(clippy::collapsible_match)]

pub mod app;
pub mod cli;
pub mod clipboard;
pub mod constants;
pub mod domain;
pub mod effect;
pub mod mcp;
pub mod models;
pub mod ollama;
pub mod prompts;
pub mod providers;
pub mod render;
pub mod runtime;
pub mod searxng;
pub mod session;
pub mod utils;

pub use app::{Config, load_config, persist_last_model};
