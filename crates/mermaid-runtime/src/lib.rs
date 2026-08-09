//! Daemon-safe runtime services.
//!
//! The TUI reducer remains the correctness core, but durable concerns
//! that a daemon or future remote client will also need live
//! here. The first slice is SQLite-backed state for tasks, approvals,
//! processes, and the future provider/memory/checkpoint tables.

pub mod apply_patch;
pub mod approval;
pub mod atomic;
pub mod checkpoint;
pub mod daemon;
pub mod git;
pub mod hardening;
mod pathguard;
pub mod plugin;
pub mod policy;
pub mod redact;
pub mod sandbox;
pub mod storage;
pub mod worktree;

/// Lowercase hex encoding of a byte slice. Shared by the SHA-256 hashing sites
/// (pairing-token hash, project hashes, plugin-source hash) that each used to
/// carry an identical private copy.
pub(crate) fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

pub use atomic::{write_atomic, write_atomic_with_mode};

pub use approval::{ApprovalReplayResult, approve_and_replay, deny_approval};
pub use checkpoint::{
    CheckpointFile, CheckpointManifest, CheckpointOrigin, create_checkpoint,
    create_checkpoint_for_task, gc_old_checkpoint_dirs, restore_checkpoint,
};
pub use daemon::{
    DEFAULT_PAIRING_TTL_DAYS, clamp_pairing_ttl_days, generate_pairing_token, hash_pairing_token,
    pairing_expiry_from_now, request_daemon_json, request_daemon_text, subscribe_daemon_lines,
};

pub use pathguard::{
    OpenIntent, create_dir_all_beneath, open_beneath, remove_file_beneath, write_atomic_beneath,
};
pub use plugin::{
    HookDecision, HookGate, HookResponse, PluginCapabilityPreview, PluginManifest,
    aggregate_hook_responses, install_plugin_from_path, plugin_capability_preview,
    run_plugin_hooks, validate_plugin_manifest, write_plugin_lockfile,
};
pub use policy::{
    ActionRequest, FloorLevel, HostShell, PLAN_DENIAL_MARKER, PolicyDecision, PolicyEngine,
    PolicyOverride, PolicyOverrideDecision, READ_ONLY_DENIAL_MARKER, RiskClass, SafetyMode,
    ToolCategory, is_destructive_command, is_plan_file_only_write, is_plan_file_path,
    is_plan_safe_build_command,
};
pub use redact::{redact_json, redact_json_text, redact_secrets, sanitize_url_for_display};
pub use sandbox::{
    Enforcement, SandboxPolicy, enforce, fs_confinement_available, network_killswitch_available,
};
pub use storage::{
    ApprovalRecord, CheckpointRecord, CompactionRecord, MessageRecord, NewApproval, NewCheckpoint,
    NewCompaction, NewOutcome, NewPluginInstall, NewProcess, NewProviderProbe, NewTask, NewToolRun,
    OUTCOME_LABEL_FAILURE, OUTCOME_LABEL_SUCCESS, OUTCOME_LABEL_UNKNOWN, OUTCOME_SOURCE_SYSTEM,
    PairingTokenRecord, PluginInstallRecord, ProcessRecord, ProcessStatus, ProviderProbeRecord,
    RuntimeStore, SessionRecord, TaskPriority, TaskRecord, TaskStatus, TaskTimelineEvent,
    ToolRunRecord, data_dir,
};
pub use worktree::{AgentWorktree, MergeOutcome, gc_orphaned_worktrees};
// Unix-only: backs the `#[cfg(unix)]` daemon singleton via `flock`.
#[cfg(unix)]
pub use storage::try_exclusive_lock;
