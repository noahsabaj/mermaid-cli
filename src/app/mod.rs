// Gateway module for app - follows the Train Station Pattern
// All external access must go through this gateway

// Private submodules - not directly accessible from outside
mod config;
pub mod event_source;
pub mod instructions;
pub mod lifecycle;
pub mod memory;
pub mod recorder;
pub mod replay;
pub mod run;
pub mod run_non_interactive;
pub mod sandbox_exec;
pub mod terminal;

// Public re-exports - the ONLY way to access app functionality
pub use config::{
    AgentTypeConfig, AgentsConfig, CompactionConfig, Config, ConfigLayer, FetchBackend,
    FilesystemPolicy, McpServerConfig, MemoryConfig, NetworkPolicy, SafetyConfig, SearchBackend,
    SessionFlags, UserProviderConfig, WebConfig, get_config_dir, init_config, load_config,
    load_config_or_warn, load_layered_config, load_layered_config_or_warn,
    persist_default_reasoning, persist_last_model, persist_ollama_allow_ram_offload,
    persist_ollama_num_ctx_for_model, persist_reasoning_for_model, remove_user_config_key,
    resolve_model_id, update_user_config_key,
};
pub use event_source::{event_to_msg, parse_slash_command};
pub use lifecycle::RuntimeLifecycle;
pub use recorder::{
    RECORDING_FORMAT_VERSION, RecordLine, Recorder, Replay, ReplayEntry, SessionHeader,
};
pub use replay::{ReplayReport, replay_recording, run_replay};
pub use run::{InteractiveOptions, run_interactive_with};
pub use run_non_interactive::{RunOptions, RunResult, format_result, run_non_interactive_with};
pub use terminal::TerminalGuard;
