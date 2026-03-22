pub mod agents;
pub mod app;
pub mod cli;
pub mod clipboard;
pub mod constants;
pub mod models;
pub mod ollama;
pub mod prompts;
pub mod runtime;
pub mod session;
pub mod tui;
pub mod utils;

pub use app::{load_config, persist_last_model, Config};
pub use models::{Model, ModelFactory};
pub use tui::run_ui;
