//! The model layer and the things it needs, and nothing above it.
//!
//! `mermaid-model` is the dependency-closed bottom of the tree: provider
//! adapters and their wire types, the capability catalog, the shared retry
//! policy, the tool-run and action value types those wire types embed, plus
//! `constants` and `utils`. Nothing here may reach up into `app`, `domain`,
//! `providers`, `render`, `effect`, or `cli` — the crate boundary is what
//! enforces that, in place of review.
//!
//! It exists because five edges pointed the wrong way. `models` read
//! `app::Config` (through a `from_app_config` nothing had ever called), reached
//! into `prompts` for a system prompt that every production path immediately
//! overwrote, borrowed `ActionDisplay` from `domain`, called the retry
//! middleware up in `effect`, and — the one that mattered — invoked
//! `ollama::ensure_running` to spawn a server from inside a wire adapter. That
//! last inversion is why a connection-refused retry sat unnoticed inside a
//! read-only listing path; recovery is now a capability the caller injects
//! (`models::adapters::ollama::LocalServerRecovery`), so a path that must not
//! start a process simply isn't given the means to.
//!
//! `mermaid-cli` re-exports every module below under its historical path
//! (`crate::models`, `crate::utils`, `crate::constants`, `domain::action`,
//! `domain::runtime`), the same shim pattern `crate::runtime` already used for
//! `mermaid-runtime`, so no call site had to change.

pub mod action;
pub mod constants;
pub mod diff;
pub mod ids;
pub mod models;
pub mod question;
pub mod tool_run;
pub mod utils;
