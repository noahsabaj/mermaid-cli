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
pub mod session;
pub mod utils;

pub use app::{Config, load_config, persist_last_model};
