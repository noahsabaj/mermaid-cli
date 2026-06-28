// Gateway module for app - follows the Train Station Pattern
// All external access must go through this gateway

// Private submodules - not directly accessible from outside
mod config;
pub mod event_source;
pub mod instructions;
pub mod lifecycle;
pub mod memory;
pub mod recorder;
pub mod run;
pub mod run_non_interactive;
pub mod terminal;

// Public re-exports - the ONLY way to access app functionality
pub use config::{
    Config, McpServerConfig, MemoryConfig, SafetyConfig, UserProviderConfig, get_config_dir,
    init_config, load_config, load_config_or_warn, persist_default_reasoning, persist_last_model,
    persist_reasoning_for_model, resolve_model_id, save_config,
};
pub use event_source::{event_to_msg, parse_slash_command};
pub use lifecycle::RuntimeLifecycle;
pub use recorder::{Recorder, Replay, ReplayEntry, record_msg_body};
pub use run::{InteractiveOptions, run_interactive, run_interactive_with};
pub use run_non_interactive::{
    RunOptions, RunResult, format_result, run_non_interactive, run_non_interactive_with,
};
pub use terminal::TerminalGuard;
