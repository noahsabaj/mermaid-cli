pub mod app;
pub mod cli;
pub mod clipboard;
pub mod domain;
pub mod effect;
pub mod mcp;
pub mod ollama;
pub mod prompts;
pub mod providers;
pub mod render;
pub mod runtime;
pub mod searxng;
pub mod session;

// `constants`, `models`, and `utils` live in the `mermaid-core` crate — the
// dependency-closed bottom of the tree, where the compiler can enforce that
// nothing in it reaches back up into `app`, `domain`, `providers`, or `effect`.
// Re-exported under their historical paths so every `crate::models::…` call
// site still resolves; same shim pattern `crate::runtime` uses for
// `mermaid-runtime`. See `mermaid_core`'s crate docs for what moved and why.
pub use mermaid_core::{constants, models, utils};

pub use app::{Config, load_config, persist_last_model};
