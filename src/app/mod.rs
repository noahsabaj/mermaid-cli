// Gateway module for app - follows the Train Station Pattern
// All external access must go through this gateway

// Private submodules - not directly accessible from outside
mod config;
mod editor;
pub mod event_source;
pub mod instructions;
pub mod lifecycle;
pub mod memory;
pub mod plugin_assets;
mod project_config;
pub mod recorder;
pub mod replay;
pub mod run;
pub mod run_non_interactive;
pub mod sandbox_exec;
pub mod skills;
pub mod terminal;

// Public re-exports - the ONLY way to access app functionality
pub use config::{
    AgentTypeConfig, AgentsConfig, CompactionConfig, Config, ConfigLayer, ExecConfig, FetchBackend,
    FilesystemPolicy, LayeredLoad, McpServerConfig, MemoryConfig, NetworkPolicy, PlanConfig,
    PlanPostApprove, SafetyConfig, SearchBackend, SessionFlags, ThemeChoice, TransportKind,
    UiConfig, UserProviderConfig, WebConfig, get_config_dir, init_config, load_config,
    load_config_or_warn, load_layered_config, load_layered_config_or_warn,
    load_project_scoped_config, persist_default_reasoning, persist_last_model,
    persist_ollama_allow_ram_offload, persist_ollama_num_ctx_for_model,
    persist_reasoning_for_model, persist_ui_theme, remove_user_config_key, resolve_model_id,
    update_user_config_key,
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

/// Impure startup backfill of session provenance — the git branch (for the
/// `--resume` picker), the git SHA at creation, and the CLI version. Done here
/// rather than in `ConversationHistory::new` so the reducer stays
/// deterministic for `--replay`. Only fills blanks: a resumed session keeps
/// what it was saved with; an older session with no stored values gets
/// backfilled on its next save. Shared by the interactive and headless paths.
pub(crate) fn stamp_session_provenance(state: &mut crate::domain::State, cwd: &std::path::Path) {
    let conversation = &mut state.session.conversation;
    if conversation.git_branch.is_none() {
        conversation.git_branch = crate::session::detect_git_branch(cwd);
    }
    if conversation.git_sha.is_none() {
        conversation.git_sha = crate::session::detect_git_sha(cwd);
    }
    if conversation.cli_version.is_none() {
        conversation.cli_version = Some(env!("CARGO_PKG_VERSION").to_string());
    }
}
