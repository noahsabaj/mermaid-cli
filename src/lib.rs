pub mod app;
pub mod cli;
pub mod clipboard;
pub mod effect;
pub mod mcp;
pub mod ollama;
pub mod providers;
pub mod render;
pub mod runtime_client;
pub mod searxng;
pub mod session;

// `constants`, `models`, and `utils` live in the `mermaid-model` crate; the
// durable runtime services live in `mermaid-runtime`. Both are named directly
// at every call site rather than re-exported under a `crate::`-local alias.
//
// The aliases used to exist so a crate extraction could land without touching
// call sites, which is what they were for. Kept past that they became a fog:
// `crate::models::` read as a local module, so nothing in a diff showed that
// `src/domain` was reaching across a crate boundary, and one file managed to
// spell the same two crates three different ways. AGENTS.md bans back-compat
// shims; these were the largest ones left.
//
// `pub use app::{Config, load_config, persist_last_model}` lived here too, with
// zero consumers — all 76 call sites used `crate::app::` directly.
