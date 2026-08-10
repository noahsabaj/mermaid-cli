//! The durable-store row DTOs: one `XRecord` for reads, one `NewX` for
//! writes, per table.
//!
//! Plain data with no behavior beyond `Default` and display labels. It
//! lives in this bottom crate because the shapes are shared vocabulary --
//! the store (`mermaid-runtime`), the daemon, the runtime client, and the
//! domain's `QueryResult`s all speak them -- and the pure MVU core must be
//! able to do so without depending on `mermaid-runtime`. Storage
//! implementation details (the schema version, owner-kind markers) stay
//! with the store.

use std::fmt;

use serde::{Deserialize, Serialize};

/// `tasks.owner_kind` value for a task the daemon runs in-process. Only these
/// are reset by the daemon's `reconcile_after_restart`; a `NULL` owner (an
/// interactive CLI run, or any other creator) is left alone so a live
/// `mermaid` session that shares the store isn't wrongly failed on daemon
/// startup (F18/RC-E).
pub const OWNER_KIND_DAEMON: &str = "daemon";

/// A stored enum label this build does not know -- the tolerant-decode error
/// for [`TaskStatus::from_db`] and friends, so one unreadable row degrades to
/// a reported error instead of sinking a whole listing.
#[derive(Debug)]
pub struct UnknownRuntimeEnum {
    kind: &'static str,
    value: String,
}

impl UnknownRuntimeEnum {
    #[must_use]
    pub fn new(kind: &'static str, value: &str) -> Self {
        Self {
            kind,
            value: value.to_string(),
        }
    }
}

impl fmt::Display for UnknownRuntimeEnum {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown {} value `{}`", self.kind, self.value)
    }
}

impl std::error::Error for UnknownRuntimeEnum {}

/// Durable task state. A task is the daemon-level work unit; a chat
/// transcript is just one artifact linked to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Queued,
    Running,
    WaitingForApproval,
    Blocked,
    Completed,
    Failed,
    Cancelled,
}

impl TaskStatus {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::WaitingForApproval => "waiting_for_approval",
            Self::Blocked => "blocked",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn from_db(value: &str) -> std::result::Result<Self, UnknownRuntimeEnum> {
        match value {
            "queued" => Ok(Self::Queued),
            "running" => Ok(Self::Running),
            "waiting_for_approval" => Ok(Self::WaitingForApproval),
            "blocked" => Ok(Self::Blocked),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            other => Err(UnknownRuntimeEnum::new("task status", other)),
        }
    }
}

impl fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskPriority {
    Low,
    Normal,
    High,
}

impl TaskPriority {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Normal => "normal",
            Self::High => "high",
        }
    }

    pub fn from_db(value: &str) -> std::result::Result<Self, UnknownRuntimeEnum> {
        match value {
            "low" => Ok(Self::Low),
            "normal" => Ok(Self::Normal),
            "high" => Ok(Self::High),
            other => Err(UnknownRuntimeEnum::new("task priority", other)),
        }
    }
}

impl fmt::Display for TaskPriority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessStatus {
    Running,
    Exited,
    Unknown,
}

impl ProcessStatus {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Exited => "exited",
            Self::Unknown => "unknown",
        }
    }

    pub fn from_db(value: &str) -> std::result::Result<Self, UnknownRuntimeEnum> {
        match value {
            "running" => Ok(Self::Running),
            "exited" => Ok(Self::Exited),
            "unknown" => Ok(Self::Unknown),
            other => Err(UnknownRuntimeEnum::new("process status", other)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRecord {
    pub id: String,
    pub title: String,
    pub status: TaskStatus,
    pub priority: TaskPriority,
    pub project_path: String,
    pub model_id: String,
    pub conversation_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub final_report: Option<String>,
    /// Full prompt for deferred daemon execution (v5). `None` for
    /// metadata-only tasks (interactive CLI runs, external `create_task`
    /// callers) — the scheduler never claims those.
    pub prompt: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskTimelineEvent {
    pub id: i64,
    pub task_id: String,
    pub kind: String,
    pub message: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub id: String,
    pub project_path: String,
    pub model_id: String,
    pub title: Option<String>,
    pub conversation_path: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub total_tokens: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSession {
    pub id: Option<String>,
    pub project_path: String,
    pub model_id: String,
    pub title: Option<String>,
    pub conversation_path: Option<String>,
    pub total_tokens: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageRecord {
    pub id: i64,
    pub session_id: String,
    pub role: String,
    pub content_json: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewMessage {
    pub session_id: String,
    pub role: String,
    pub content_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewTask {
    pub title: String,
    pub project_path: String,
    pub model_id: String,
    pub priority: TaskPriority,
    pub conversation_id: Option<String>,
    /// Which kind of process owns this task. `Some("daemon")` (set via
    /// [`Self::daemon_owned`]) marks a task the daemon runs in-process, so the
    /// startup reconcile may fail it if a crash left it `Running`. `None` — the
    /// default, used by interactive CLI runs and any other creator — is left
    /// untouched by reconcile so a live session isn't clobbered (F18/RC-E).
    pub owner_kind: Option<String>,
    /// Full prompt for deferred execution by the daemon scheduler. Tasks
    /// without one are metadata-only and are never claimed.
    pub prompt: Option<String>,
}

impl NewTask {
    pub fn new(
        title: impl Into<String>,
        project_path: impl Into<String>,
        model_id: impl Into<String>,
    ) -> Self {
        Self {
            title: title.into(),
            project_path: project_path.into(),
            model_id: model_id.into(),
            priority: TaskPriority::Normal,
            conversation_id: None,
            owner_kind: None,
            prompt: None,
        }
    }

    /// Mark this task as daemon-owned (run in the daemon process). Only such
    /// tasks are reset by [`RuntimeStore::reconcile_after_restart`]; omit it for
    /// interactive CLI runs so they survive a daemon restart.
    #[must_use]
    pub fn daemon_owned(mut self) -> Self {
        self.owner_kind = Some(OWNER_KIND_DAEMON.to_string());
        self
    }

    /// Persist the full prompt so the scheduler can execute this task later.
    pub fn with_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.prompt = Some(prompt.into());
        self
    }

    #[must_use]
    pub fn with_priority(mut self, priority: TaskPriority) -> Self {
        self.priority = priority;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRecord {
    pub id: String,
    pub task_id: Option<String>,
    pub proposed_action: String,
    pub risk_classification: String,
    pub policy_decision: String,
    pub user_decision: Option<String>,
    pub args_summary: Option<String>,
    pub checkpoint_id: Option<String>,
    pub pending_action_json: Option<String>,
    pub created_at: String,
    pub decided_at: Option<String>,
    pub archived_at: Option<String>,
    pub archive_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewApproval {
    pub task_id: Option<String>,
    pub proposed_action: String,
    pub risk_classification: String,
    pub policy_decision: String,
    pub args_summary: Option<String>,
    pub checkpoint_id: Option<String>,
    pub pending_action_json: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolRunRecord {
    pub id: String,
    pub task_id: Option<String>,
    pub turn_id: Option<String>,
    pub call_id: Option<String>,
    pub tool_name: String,
    pub status: String,
    pub args_json: Option<String>,
    pub output_json: Option<String>,
    pub started_at: String,
    pub finished_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewToolRun {
    pub id: Option<String>,
    pub task_id: Option<String>,
    pub turn_id: Option<String>,
    pub call_id: Option<String>,
    pub tool_name: String,
    pub args_json: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessRecord {
    pub id: String,
    pub task_id: Option<String>,
    pub pid: u32,
    pub command: String,
    pub cwd: Option<String>,
    pub log_path: Option<String>,
    pub detected_url: Option<String>,
    pub status: ProcessStatus,
    pub health: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewProcess {
    pub id: Option<String>,
    pub task_id: Option<String>,
    pub pid: u32,
    pub command: String,
    pub cwd: Option<String>,
    pub log_path: Option<String>,
    pub detected_url: Option<String>,
    pub status: ProcessStatus,
    pub health: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointRecord {
    pub id: String,
    pub task_id: Option<String>,
    pub project_path: String,
    pub snapshot_path: String,
    pub changed_files_json: String,
    pub pending_action_json: Option<String>,
    pub approval_id: Option<String>,
    pub created_at: String,
    pub archived_at: Option<String>,
    pub archive_reason: Option<String>,
    /// Conversation the checkpointed mutation belonged to, when the tool call
    /// ran inside an interactive session. `None` for headless/daemon/manual
    /// checkpoints.
    pub session_id: Option<String>,
    /// Conversation length (`messages().len()`) at tool DISPATCH. A rewind
    /// that forks at user-message index `k` keeps `messages[..k]`, so this
    /// checkpoint belongs to the discarded timeline iff `message_index > k`
    /// (STRICT — see `list_for_session`).
    pub message_index: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewCheckpoint {
    pub id: Option<String>,
    pub task_id: Option<String>,
    pub project_path: String,
    pub snapshot_path: String,
    pub changed_files_json: String,
    pub pending_action_json: Option<String>,
    pub approval_id: Option<String>,
    pub session_id: Option<String>,
    pub message_index: Option<i64>,
}

/// Provenance of an [`OutcomeRecord`] — the axis that separates a genuine
/// external training signal from model self-judgement. `verifier` (compiler,
/// tests, runtime) and `user` (human edit/accept/reject) are the signals that
/// can actually improve a model; `model` is self-judged and must never be
/// trained on unfiltered; `system` is bookkeeping (e.g. a task's terminal
/// status).
pub const OUTCOME_SOURCE_VERIFIER: &str = "verifier";
pub const OUTCOME_SOURCE_USER: &str = "user";
pub const OUTCOME_SOURCE_MODEL: &str = "model";
pub const OUTCOME_SOURCE_SYSTEM: &str = "system";

/// Graded result of an outcome. Stored as free-form `TEXT` (like
/// `tool_runs.status`) so the taxonomy can grow without a migration; these
/// constants are the canonical spellings so callers don't drift.
pub const OUTCOME_LABEL_SUCCESS: &str = "success";
pub const OUTCOME_LABEL_FAILURE: &str = "failure";
pub const OUTCOME_LABEL_PARTIAL: &str = "partial";
pub const OUTCOME_LABEL_ACCEPTED: &str = "accepted";
pub const OUTCOME_LABEL_REJECTED: &str = "rejected";
pub const OUTCOME_LABEL_UNKNOWN: &str = "unknown";

/// A verifiable outcome / reward signal attached to a trajectory (a task, and
/// optionally a specific tool run). The other durable tables record *what
/// happened* (messages, `tool_runs`, checkpoints=diffs); `outcomes` records *how
/// good it was* and *who says so* ([`source`](Self::source)) — the enrichment
/// that turns logs into a training set for the self-improving loop.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutcomeRecord {
    pub id: String,
    pub task_id: Option<String>,
    pub tool_run_id: Option<String>,
    /// Signal type, e.g. `task_terminal`, `build`, `test`, `tool_exec`,
    /// `user_edit`, `git_survival`, `preference`. Free-form.
    pub kind: String,
    /// Graded result — one of the `OUTCOME_LABEL_*` values.
    pub label: String,
    /// Optional scalar reward (convention: roughly `-1.0..=1.0`). `None` when
    /// the signal is categorical only.
    pub reward: Option<f64>,
    /// Provenance — one of the `OUTCOME_SOURCE_*` values.
    pub source: String,
    /// Optional structured payload: test counts, a git sha, or a preference
    /// pair `{ "chosen": ..., "rejected": ... }` for DPO.
    pub detail_json: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewOutcome {
    pub id: Option<String>,
    pub task_id: Option<String>,
    pub tool_run_id: Option<String>,
    pub kind: String,
    pub label: String,
    pub reward: Option<f64>,
    pub source: String,
    pub detail_json: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionRecord {
    pub id: String,
    pub task_id: Option<String>,
    pub session_id: Option<String>,
    pub source_token_estimate: Option<i64>,
    pub summary_token_count: Option<i64>,
    pub preserved_turns: Option<i64>,
    pub archive_path: Option<String>,
    pub verification_status: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewCompaction {
    pub id: Option<String>,
    pub task_id: Option<String>,
    pub session_id: Option<String>,
    pub source_token_estimate: Option<i64>,
    pub summary_token_count: Option<i64>,
    pub preserved_turns: Option<i64>,
    pub archive_path: Option<String>,
    pub verification_status: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginInstallRecord {
    pub id: String,
    pub name: String,
    pub source: String,
    pub version: Option<String>,
    pub enabled: bool,
    pub manifest_json: String,
    pub installed_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPluginInstall {
    pub id: Option<String>,
    pub name: String,
    pub source: String,
    pub version: Option<String>,
    pub enabled: bool,
    pub manifest_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderProbeRecord {
    pub provider: String,
    pub model_id: String,
    pub capability_key: String,
    pub capability_value: String,
    pub confidence: String,
    pub error: Option<String>,
    pub probed_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewProviderProbe {
    pub provider: String,
    pub model_id: String,
    pub capability_key: String,
    pub capability_value: String,
    pub confidence: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingTokenRecord {
    pub id: String,
    pub token_hash: String,
    pub label: Option<String>,
    pub enabled: bool,
    pub created_at: String,
    pub last_used_at: Option<String>,
    /// RFC3339 expiry. `None` = never expires (opt-in via `--ttl-days 0`).
    pub expires_at: Option<String>,
}
