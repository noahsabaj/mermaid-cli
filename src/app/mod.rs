// Gateway module for app - follows the Train Station Pattern
// All external access must go through this gateway

// Private submodules - not directly accessible from outside
mod config;
pub mod event_source;
pub mod instructions;
pub mod recorder;
pub mod run;
pub mod terminal;

// Public re-exports - the ONLY way to access app functionality
pub use config::{
    Config, McpServerConfig, UserProviderConfig, get_config_dir, init_config, load_config,
    persist_default_reasoning, persist_last_model, persist_reasoning_for_model, resolve_model_id,
    save_config,
};
pub use event_source::{event_to_msg, parse_slash_command};
pub use recorder::{Recorder, Replay, ReplayEntry};
pub use run::{opted_in as v7_opted_in, run as run_v7};
pub use terminal::TerminalGuard;
