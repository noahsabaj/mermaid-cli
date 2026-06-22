//! Daemon-safe runtime services.
//!
//! The TUI reducer remains the correctness core, but durable concerns
//! that a daemon or future remote client will also need live
//! here. The first slice is SQLite-backed state for tasks, approvals,
//! processes, and the future provider/memory/checkpoint tables.

pub mod approval;
pub mod checkpoint;
pub mod daemon;
pub mod plugin;
pub mod policy;
pub mod storage;

pub use approval::{ApprovalReplayResult, approve_and_replay, deny_approval};
pub use checkpoint::{
    CheckpointFile, CheckpointManifest, create_checkpoint, create_checkpoint_for_task,
    restore_checkpoint,
};
pub use daemon::{
    daemon_socket_path, generate_pairing_token, hash_pairing_token, request_daemon_json,
    request_daemon_text, snapshot_field_from_daemon,
};
pub use plugin::{
    PluginManifest, PluginPermissionPreview, install_plugin_from_path, plugin_permission_preview,
    run_plugin_hooks, validate_plugin_manifest, write_plugin_lockfile,
};
pub use policy::{
    ActionRequest, PolicyDecision, PolicyEngine, PolicyOverride, PolicyOverrideDecision, RiskClass,
    SafetyMode, ToolCategory,
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
