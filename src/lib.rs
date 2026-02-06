pub mod agents;
pub mod app;
pub mod cache;
pub mod cli;
pub mod constants;
pub mod context;
pub mod models;
pub mod ollama;
pub mod prompts;
pub mod runtime;
pub mod session;
pub mod tui;
pub mod utils;

pub use app::{load_config, persist_last_model, Config};
pub use context::{Context, ContextConfig};
pub use models::{Model, ModelFactory, ProjectContext};
pub use tui::run_ui;
