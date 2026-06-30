//! Daemon-safe runtime services.
//!
//! The TUI reducer remains the correctness core, but durable concerns
//! that a daemon or future remote client will also need live
//! here. The first slice is SQLite-backed state for tasks, approvals,
//! processes, and the future provider/memory/checkpoint tables.

pub mod approval;
pub mod atomic;
pub mod checkpoint;
pub mod daemon;
mod pathguard;
pub mod plugin;
pub mod policy;
pub mod storage;

pub use atomic::write_atomic;

pub use approval::{ApprovalReplayResult, approve_and_replay, deny_approval};
pub use checkpoint::{
    CheckpointFile, CheckpointManifest, create_checkpoint, create_checkpoint_for_task,
    gc_old_checkpoint_dirs, restore_checkpoint,
};
pub use daemon::{
    DEFAULT_PAIRING_TTL_DAYS, clamp_pairing_ttl_days, daemon_socket_path, generate_pairing_token,
    hash_pairing_token, pairing_expiry_from_now, request_daemon_json, request_daemon_text,
    snapshot_field_from_daemon,
};
pub use pathguard::{
    OpenIntent, create_dir_all_beneath, open_beneath, remove_file_beneath, write_atomic_beneath,
};
pub use plugin::{
    PluginCapabilityPreview, PluginManifest, install_plugin_from_path, plugin_capability_preview,
    run_plugin_hooks, validate_plugin_manifest, write_plugin_lockfile,
};
pub use policy::{
    ActionRequest, PolicyDecision, PolicyEngine, PolicyOverride, PolicyOverrideDecision, RiskClass,
    SafetyMode, ToolCategory, is_destructive_command,
};
pub use storage::{
    ApprovalRecord, ApprovalsRepo, CheckpointRecord, CheckpointsRepo, CompactionRecord,
    CompactionsRepo, MessageRecord, MessagesRepo, NewApproval, NewCheckpoint, NewCompaction,
    NewMessage, NewPluginInstall, NewProcess, NewProviderProbe, NewSession, NewTask, NewToolRun,
    PairingTokenRecord, PairingTokensRepo, PluginInstallRecord, PluginsRepo, ProcessRecord,
    ProcessStatus, ProcessesRepo, ProviderProbeRecord, ProviderProbesRepo, RuntimeStore,
    SessionRecord, SessionsRepo, TaskPriority, TaskRecord, TaskStatus, TaskTimelineEvent,
    TasksRepo, ToolRunRecord, ToolRunsRepo, data_dir,
};
// Unix-only: backs the `#[cfg(unix)]` daemon singleton via `flock`.
#[cfg(unix)]
pub use storage::try_exclusive_lock;
