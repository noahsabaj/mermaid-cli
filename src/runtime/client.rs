use std::collections::HashSet;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

use super::{
    ApprovalRecord, ApprovalReplayResult, CheckpointManifest, CheckpointRecord, CompactionRecord,
    MessageRecord, NewProcess, NewProviderProbe, PairingTokenRecord, PluginInstallRecord,
    ProcessRecord, ProcessStatus, ProviderProbeRecord, RuntimeStore, SessionRecord, TaskRecord,
    TaskStatus, TaskTimelineEvent, ToolRunRecord, approve_and_replay, deny_approval,
    request_daemon_json, restore_checkpoint,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeClientSource {
    Daemon,
    Local,
}

impl RuntimeClientSource {
    pub fn as_str(self) -> &'static str {
        match self {
            RuntimeClientSource::Daemon => "daemon",
            RuntimeClientSource::Local => "local",
        }
    }
}

#[derive(Debug, Clone)]
enum RuntimeClientMode {
    PreferDaemon,
    DaemonOnly,
    LocalOnly,
}

#[derive(Debug, Clone)]
pub struct RuntimeClient {
    mode: RuntimeClientMode,
    auth_token: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeRead<T> {
    pub source: RuntimeClientSource,
    pub value: T,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeHealth {
    pub ok: bool,
    pub service: String,
    pub database: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeSnapshot {
    pub ok: bool,
    pub database: String,
    pub sessions: Vec<SessionRecord>,
    pub tasks: Vec<TaskRecord>,
    pub tool_runs: Vec<ToolRunRecord>,
    pub processes: Vec<ProcessRecord>,
    pub approvals: Vec<ApprovalRecord>,
    pub checkpoints: Vec<CheckpointRecord>,
    pub compactions: Vec<CompactionRecord>,
    pub plugins: Vec<PluginInstallRecord>,
    pub provider_probes: Vec<ProviderProbeRecord>,
    pub pairings: Vec<PairingTokenRecord>,
    pub safety: crate::app::SafetyConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeDashboardCounts {
    pub pending_approvals: usize,
    pub running_tasks: usize,
    pub waiting_tasks: usize,
    pub blocked_tasks: usize,
    pub ready_processes: usize,
    pub recent_checkpoints: usize,
    pub installed_plugins: usize,
    pub archived_approvals: usize,
    pub archived_checkpoints: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeDashboard {
    pub ok: bool,
    pub health: RuntimeHealth,
    pub safety: crate::app::SafetyConfig,
    pub counts: RuntimeDashboardCounts,
    pub sessions: Vec<SessionRecord>,
    pub tasks: Vec<TaskRecord>,
    pub tool_runs: Vec<ToolRunRecord>,
    pub processes: Vec<ProcessRecord>,
    pub approvals: Vec<ApprovalRecord>,
    pub checkpoints: Vec<CheckpointRecord>,
    pub compactions: Vec<CompactionRecord>,
    pub plugins: Vec<PluginInstallRecord>,
    pub provider_probes: Vec<ProviderProbeRecord>,
    pub pairings: Vec<PairingTokenRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeDiagnostics {
    #[serde(flatten)]
    pub snapshot: RuntimeSnapshot,
    pub mode: String,
    pub hygiene: RuntimeDiagnosticsHygiene,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeDiagnosticsHygiene {
    pub preview: Value,
    pub visible: RuntimeDiagnosticsVisible,
    pub archived: RuntimeDiagnosticsArchived,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeDiagnosticsVisible {
    pub approvals: usize,
    pub checkpoints: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeDiagnosticsArchived {
    pub approvals: usize,
    pub checkpoints: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeHygienePreview {
    pub ok: bool,
    pub reason: String,
    pub approvals: Vec<ApprovalRecord>,
    pub checkpoints: Vec<CheckpointRecord>,
    pub counts: RuntimeHygieneCounts,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeHygieneCounts {
    pub approvals: usize,
    pub checkpoints: usize,
    pub total: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeHygieneArchive {
    pub ok: bool,
    pub reason: String,
    pub archived: RuntimeHygieneCounts,
    pub matched: RuntimeHygieneCounts,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeTaskDetail {
    pub ok: bool,
    pub task: TaskRecord,
    pub events: Vec<TaskTimelineEvent>,
    pub session: Option<SessionRecord>,
    pub messages: Vec<MessageRecord>,
    pub approvals: Vec<ApprovalRecord>,
    pub checkpoints: Vec<CheckpointRecord>,
    pub processes: Vec<ProcessRecord>,
    pub tool_runs: Vec<ToolRunRecord>,
    pub compactions: Vec<CompactionRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeApprovalDetail {
    pub ok: bool,
    pub approval: ApprovalRecord,
    pub task: Option<TaskRecord>,
    pub checkpoint: Option<CheckpointRecord>,
    pub pending_action: Value,
    pub args: Value,
    pub changed_files: Value,
    pub affected_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeCheckpointDetail {
    pub ok: bool,
    pub checkpoint: CheckpointRecord,
    pub task: Option<TaskRecord>,
    pub approval: Option<ApprovalRecord>,
    pub pending_action: Value,
    pub changed_files: Value,
    pub affected_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeProcessLog {
    pub ok: bool,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeProcessOpen {
    pub ok: bool,
    pub target: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimePorts {
    pub ok: bool,
    pub ports: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeApprovalDecision {
    pub ok: bool,
    pub approval: Option<ApprovalRecord>,
    pub replayed: bool,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeCheckpointRestore {
    pub ok: bool,
    pub checkpoint: CheckpointManifest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RuntimeItems<T> {
    ok: bool,
    items: Vec<T>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeOne<T> {
    pub ok: bool,
    pub item: T,
}

pub struct RuntimeService {
    store: RuntimeStore,
}

impl RuntimeClient {
    pub fn auto() -> Self {
        Self {
            mode: RuntimeClientMode::PreferDaemon,
            auth_token: None,
        }
    }

    pub fn daemon() -> Self {
        Self {
            mode: RuntimeClientMode::DaemonOnly,
            auth_token: None,
        }
    }

    pub fn daemon_with_token(token: impl Into<String>) -> Self {
        Self {
            mode: RuntimeClientMode::DaemonOnly,
            auth_token: Some(token.into()),
        }
    }

    pub fn local() -> Self {
        Self {
            mode: RuntimeClientMode::LocalOnly,
            auth_token: None,
        }
    }

    pub fn health(&self) -> Result<RuntimeRead<RuntimeHealth>> {
        self.read(json!({"command": "health"}), |service| service.health())
    }

    pub fn snapshot(&self) -> Result<RuntimeRead<RuntimeSnapshot>> {
        self.read(json!({"command": "runtime_snapshot"}), |service| {
            service.snapshot()
        })
    }

    pub fn dashboard(&self) -> Result<RuntimeRead<RuntimeDashboard>> {
        self.read(json!({"command": "runtime_dashboard"}), |service| {
            service.dashboard()
        })
    }

    pub fn diagnostics(&self) -> Result<RuntimeRead<RuntimeDiagnostics>> {
        self.read(json!({"command": "runtime_diagnostics"}), |service| {
            service.diagnostics()
        })
    }

    pub fn hygiene_preview(&self) -> Result<RuntimeRead<RuntimeHygienePreview>> {
        self.read(json!({"command": "runtime_hygiene_preview"}), |service| {
            service.hygiene_preview()
        })
    }

    pub fn hygiene_archive(&self) -> Result<RuntimeRead<RuntimeHygieneArchive>> {
        self.read_inner(
            json!({"command": "runtime_hygiene_archive"}),
            true,
            |service| service.hygiene_archive(),
        )
    }

    pub fn task_detail(&self, id: &str) -> Result<RuntimeRead<RuntimeTaskDetail>> {
        self.read(
            json!({"command": "runtime_task_detail", "id": id}),
            |service| service.task_detail(id),
        )
    }

    pub fn approval_detail(&self, id: &str) -> Result<RuntimeRead<RuntimeApprovalDetail>> {
        self.read(
            json!({"command": "runtime_approval_detail", "id": id}),
            |service| service.approval_detail(id),
        )
    }

    pub fn checkpoint_detail(&self, id: &str) -> Result<RuntimeRead<RuntimeCheckpointDetail>> {
        self.read(
            json!({"command": "runtime_checkpoint_detail", "id": id}),
            |service| service.checkpoint_detail(id),
        )
    }

    pub fn list_tasks(&self, limit: usize) -> Result<RuntimeRead<Vec<TaskRecord>>> {
        self.list(
            json!({"command": "runtime_tasks", "limit": limit}),
            |service| service.list_tasks(limit),
        )
    }

    pub fn list_processes(&self, limit: usize) -> Result<RuntimeRead<Vec<ProcessRecord>>> {
        self.list(
            json!({"command": "runtime_processes", "limit": limit}),
            |service| service.list_processes(limit),
        )
    }

    pub fn list_approvals(&self) -> Result<RuntimeRead<Vec<ApprovalRecord>>> {
        self.list(json!({"command": "runtime_approvals"}), |service| {
            service.list_approvals()
        })
    }

    pub fn list_tool_runs(&self, limit: usize) -> Result<RuntimeRead<Vec<ToolRunRecord>>> {
        self.list(
            json!({"command": "runtime_tool_runs", "limit": limit}),
            |service| service.list_tool_runs(limit),
        )
    }

    pub fn list_checkpoints(&self, limit: usize) -> Result<RuntimeRead<Vec<CheckpointRecord>>> {
        self.list(
            json!({"command": "runtime_checkpoints", "limit": limit}),
            |service| service.list_checkpoints(limit),
        )
    }

    pub fn list_plugins(&self) -> Result<RuntimeRead<Vec<PluginInstallRecord>>> {
        self.list(json!({"command": "runtime_plugins"}), |service| {
            service.list_plugins()
        })
    }

    pub fn process_log(&self, id: &str, tail_bytes: Option<u64>) -> Result<RuntimeProcessLog> {
        self.action(
            json!({
                "command": "logs",
                "id": id,
                "tail_bytes": tail_bytes,
            }),
            |service| service.process_log(id, tail_bytes),
        )
    }

    pub fn stop_process(&self, id: &str) -> Result<RuntimeOne<ProcessRecord>> {
        self.action_authed(json!({"command": "stop_process", "id": id}), |service| {
            service
                .stop_process(id)
                .map(|item| RuntimeOne { ok: true, item })
        })
    }

    pub fn restart_process(&self, id: &str) -> Result<RuntimeOne<ProcessRecord>> {
        self.action_authed(json!({"command": "restart_process", "id": id}), |service| {
            service
                .restart_process(id)
                .map(|item| RuntimeOne { ok: true, item })
        })
    }

    pub fn open_process(&self, id: &str) -> Result<RuntimeProcessOpen> {
        self.action_authed(json!({"command": "open_process", "id": id}), |service| {
            service.open_process(id)
        })
    }

    pub fn ports(&self) -> Result<RuntimePorts> {
        self.action(json!({"command": "ports"}), |service| service.ports())
    }

    pub fn approve(&self, id: &str) -> Result<RuntimeApprovalDecision> {
        self.action_authed(json!({"command": "approve", "id": id}), |_service| {
            RuntimeService::approval_decision(approve_and_replay(id)?)
        })
    }

    pub fn deny(&self, id: &str) -> Result<RuntimeApprovalDecision> {
        self.action_authed(json!({"command": "deny", "id": id}), |_service| {
            RuntimeService::approval_decision(deny_approval(id)?)
        })
    }

    pub fn restore_checkpoint(&self, id: &str) -> Result<RuntimeCheckpointRestore> {
        self.action_authed(
            json!({"command": "restore_checkpoint", "id": id}),
            |_service| {
                Ok(RuntimeCheckpointRestore {
                    ok: true,
                    checkpoint: restore_checkpoint(id)?,
                })
            },
        )
    }

    pub fn set_plugin_enabled(&self, id: &str, enabled: bool) -> Result<()> {
        self.action_authed::<Value, _>(
            json!({"command": "set_plugin_enabled", "id": id, "enabled": enabled}),
            |service| {
                service.set_plugin_enabled(id, enabled)?;
                Ok(json!({"ok": true}))
            },
        )?;
        Ok(())
    }

    pub fn model_info(&self, model: &str) -> Result<Value> {
        self.action(
            json!({"command": "model_info", "model": model}),
            |service| Ok(json!({"ok": true, "model": service.model_info(model)})),
        )
    }

    pub fn set_safety_mode(&self, mode: &str) -> Result<Value> {
        self.action_authed(
            json!({"command": "set_safety_mode", "mode": mode}),
            |service| {
                let safety = service.set_safety_mode(mode)?;
                Ok(json!({"ok": true, "safety": safety}))
            },
        )
    }

    fn list<T, F>(&self, body: Value, local: F) -> Result<RuntimeRead<Vec<T>>>
    where
        T: DeserializeOwned,
        F: FnOnce(&RuntimeService) -> Result<Vec<T>>,
    {
        self.read(body, |service| {
            local(service).map(|items| RuntimeItems { ok: true, items })
        })
        .map(|read| RuntimeRead {
            source: read.source,
            value: read.value.items,
        })
    }

    fn read<T, F>(&self, body: Value, local: F) -> Result<RuntimeRead<T>>
    where
        T: DeserializeOwned,
        F: FnOnce(&RuntimeService) -> Result<T>,
    {
        self.read_inner(body, false, local)
    }

    fn action<T, F>(&self, body: Value, local: F) -> Result<T>
    where
        T: DeserializeOwned,
        F: FnOnce(&RuntimeService) -> Result<T>,
    {
        self.action_inner(body, false, local)
    }

    fn action_authed<T, F>(&self, body: Value, local: F) -> Result<T>
    where
        T: DeserializeOwned,
        F: FnOnce(&RuntimeService) -> Result<T>,
    {
        self.action_inner(body, true, local)
    }

    fn read_inner<T, F>(&self, body: Value, authed: bool, local: F) -> Result<RuntimeRead<T>>
    where
        T: DeserializeOwned,
        F: FnOnce(&RuntimeService) -> Result<T>,
    {
        match self.mode {
            RuntimeClientMode::LocalOnly => RuntimeService::open_default()
                .and_then(|service| local(&service))
                .map(|value| RuntimeRead {
                    source: RuntimeClientSource::Local,
                    value,
                }),
            RuntimeClientMode::DaemonOnly => {
                self.request_daemon(body, authed).map(|value| RuntimeRead {
                    source: RuntimeClientSource::Daemon,
                    value,
                })
            },
            RuntimeClientMode::PreferDaemon => match self.request_daemon(body, authed) {
                Ok(value) => Ok(RuntimeRead {
                    source: RuntimeClientSource::Daemon,
                    value,
                }),
                Err(_) => RuntimeService::open_default()
                    .and_then(|service| local(&service))
                    .map(|value| RuntimeRead {
                        source: RuntimeClientSource::Local,
                        value,
                    }),
            },
        }
    }

    fn action_inner<T, F>(&self, body: Value, authed: bool, local: F) -> Result<T>
    where
        T: DeserializeOwned,
        F: FnOnce(&RuntimeService) -> Result<T>,
    {
        match self.mode {
            RuntimeClientMode::LocalOnly => {
                RuntimeService::open_default().and_then(|service| local(&service))
            },
            RuntimeClientMode::DaemonOnly => self.request_daemon(body, authed),
            RuntimeClientMode::PreferDaemon => match self.request_daemon(body, authed) {
                Ok(value) => Ok(value),
                Err(_) => RuntimeService::open_default().and_then(|service| local(&service)),
            },
        }
    }

    fn request_daemon<T: DeserializeOwned>(&self, mut body: Value, authed: bool) -> Result<T> {
        if authed && let Some(token) = self.auth_token.as_ref() {
            body["auth"] = json!({ "token": token });
        }
        let value = request_daemon_json(body)?;
        serde_json::from_value(value).context("daemon response had unexpected shape")
    }
}

impl RuntimeService {
    pub fn open_default() -> Result<Self> {
        Ok(Self {
            store: RuntimeStore::open_default()?,
        })
    }

    pub fn from_store(store: RuntimeStore) -> Self {
        Self { store }
    }

    pub fn health(&self) -> Result<RuntimeHealth> {
        Ok(RuntimeHealth {
            ok: true,
            service: "mermaidd".to_string(),
            database: self.store.path().display().to_string(),
        })
    }

    pub fn snapshot(&self) -> Result<RuntimeSnapshot> {
        Ok(RuntimeSnapshot {
            ok: true,
            database: self.store.path().display().to_string(),
            sessions: self.store.sessions().list(100)?,
            tasks: self.store.tasks().list(100)?,
            tool_runs: self.store.tool_runs().list(100)?,
            processes: self.store.processes().list(100)?,
            approvals: self.store.approvals().list_all(100)?,
            checkpoints: self.store.checkpoints().list_all(100)?,
            compactions: self.store.compactions().list(100)?,
            plugins: self.store.plugins().list()?,
            provider_probes: self.store.provider_probes().list(None, None)?,
            // Redact token hashes — this snapshot is served over the local
            // socket to same-UID processes without auth.
            pairings: self.store.pairing_tokens().list_redacted()?,
            safety: crate::app::load_config().unwrap_or_default().safety,
        })
    }

    pub fn dashboard(&self) -> Result<RuntimeDashboard> {
        let sessions = self.store.sessions().list(100)?;
        let tasks = self.store.tasks().list(200)?;
        let tool_runs = self.store.tool_runs().list(100)?;
        let processes = self.store.processes().list(100)?;
        let approvals = self.store.approvals().list_pending()?;
        let checkpoints = self.store.checkpoints().list(100)?;
        let compactions = self.store.compactions().list(100)?;
        let plugins = self.store.plugins().list()?;
        let provider_probes = self.store.provider_probes().list(None, None)?;
        let pairings = self.store.pairing_tokens().list_redacted()?;

        let running_tasks = tasks
            .iter()
            .filter(|task| task.status == TaskStatus::Running)
            .count();
        let waiting_tasks = tasks
            .iter()
            .filter(|task| task.status == TaskStatus::WaitingForApproval)
            .count();
        let blocked_tasks = tasks
            .iter()
            .filter(|task| matches!(task.status, TaskStatus::Blocked | TaskStatus::Failed))
            .count();
        let ready_processes = processes
            .iter()
            .filter(|process| process.detected_url.is_some())
            .count();

        Ok(RuntimeDashboard {
            ok: true,
            health: self.health()?,
            safety: crate::app::load_config().unwrap_or_default().safety,
            counts: RuntimeDashboardCounts {
                pending_approvals: approvals.len(),
                running_tasks,
                waiting_tasks,
                blocked_tasks,
                ready_processes,
                recent_checkpoints: checkpoints.len(),
                installed_plugins: plugins.len(),
                archived_approvals: self.store.approvals().count_archived()?,
                archived_checkpoints: self.store.checkpoints().count_archived()?,
            },
            sessions,
            tasks,
            tool_runs,
            processes,
            approvals,
            checkpoints,
            compactions,
            plugins,
            provider_probes,
            pairings,
        })
    }

    pub fn diagnostics(&self) -> Result<RuntimeDiagnostics> {
        let snapshot = self.snapshot()?;
        let hygiene = self.hygiene_preview()?;
        Ok(RuntimeDiagnostics {
            snapshot,
            mode: "diagnostics".to_string(),
            hygiene: RuntimeDiagnosticsHygiene {
                preview: serde_json::to_value(hygiene)?,
                visible: RuntimeDiagnosticsVisible {
                    approvals: self.store.approvals().list_pending()?.len(),
                    checkpoints: self.store.checkpoints().list(100)?.len(),
                },
                archived: RuntimeDiagnosticsArchived {
                    approvals: self.store.approvals().count_archived()?,
                    checkpoints: self.store.checkpoints().count_archived()?,
                },
            },
        })
    }

    pub fn hygiene_preview(&self) -> Result<RuntimeHygienePreview> {
        let preview = self.raw_hygiene_preview()?;
        Ok(RuntimeHygienePreview {
            ok: true,
            reason: runtime_hygiene_reason().to_string(),
            counts: RuntimeHygieneCounts {
                approvals: preview.approvals.len(),
                checkpoints: preview.checkpoints.len(),
                total: preview.approvals.len() + preview.checkpoints.len(),
            },
            approvals: preview.approvals,
            checkpoints: preview.checkpoints,
        })
    }

    pub fn hygiene_archive(&self) -> Result<RuntimeHygieneArchive> {
        let preview = self.raw_hygiene_preview()?;
        let approval_ids = preview
            .approvals
            .iter()
            .map(|approval| approval.id.clone())
            .collect::<Vec<_>>();
        let checkpoint_ids = preview
            .checkpoints
            .iter()
            .map(|checkpoint| checkpoint.id.clone())
            .collect::<Vec<_>>();
        let reason = runtime_hygiene_reason();
        let approvals_archived = self.store.approvals().archive(&approval_ids, reason)?;
        let checkpoints_archived = self.store.checkpoints().archive(&checkpoint_ids, reason)?;

        Ok(RuntimeHygieneArchive {
            ok: true,
            reason: reason.to_string(),
            archived: RuntimeHygieneCounts {
                approvals: approvals_archived,
                checkpoints: checkpoints_archived,
                total: approvals_archived + checkpoints_archived,
            },
            matched: RuntimeHygieneCounts {
                approvals: approval_ids.len(),
                checkpoints: checkpoint_ids.len(),
                total: approval_ids.len() + checkpoint_ids.len(),
            },
        })
    }

    pub fn task_detail(&self, id: &str) -> Result<RuntimeTaskDetail> {
        let task = self
            .store
            .tasks()
            .get(id)?
            .with_context(|| format!("task not found: {}", id))?;
        let events = self.store.tasks().events(id)?;
        let session = match task.conversation_id.as_deref() {
            Some(session_id) => self.store.sessions().get(session_id)?,
            None => None,
        };
        let messages = match task.conversation_id.as_deref() {
            Some(session_id) => self.store.messages().list_for_session(session_id)?,
            None => Vec::new(),
        };
        let approvals = self
            .store
            .approvals()
            .list_pending()?
            .into_iter()
            .filter(|approval| approval.task_id.as_deref() == Some(id))
            .collect::<Vec<_>>();
        let checkpoints = self
            .store
            .checkpoints()
            .list(200)?
            .into_iter()
            .filter(|checkpoint| checkpoint.task_id.as_deref() == Some(id))
            .collect::<Vec<_>>();
        let processes = self
            .store
            .processes()
            .list(200)?
            .into_iter()
            .filter(|process| process.task_id.as_deref() == Some(id))
            .collect::<Vec<_>>();
        let tool_runs = self
            .store
            .tool_runs()
            .list(200)?
            .into_iter()
            .filter(|tool_run| tool_run.task_id.as_deref() == Some(id))
            .collect::<Vec<_>>();
        let compactions = self
            .store
            .compactions()
            .list(200)?
            .into_iter()
            .filter(|compaction| compaction.task_id.as_deref() == Some(id))
            .collect::<Vec<_>>();

        Ok(RuntimeTaskDetail {
            ok: true,
            task,
            events,
            session,
            messages,
            approvals,
            checkpoints,
            processes,
            tool_runs,
            compactions,
        })
    }

    pub fn approval_detail(&self, id: &str) -> Result<RuntimeApprovalDetail> {
        let approval = self
            .store
            .approvals()
            .get(id)?
            .with_context(|| format!("approval not found: {}", id))?;
        let checkpoint = match approval.checkpoint_id.as_deref() {
            Some(checkpoint_id) => self.store.checkpoints().get(checkpoint_id)?,
            None => None,
        };
        let task = match approval.task_id.as_deref() {
            Some(task_id) => self.store.tasks().get(task_id)?,
            None => None,
        };
        let pending_action = parse_optional_json(approval.pending_action_json.as_deref());
        let args = parse_optional_json(approval.args_summary.as_deref());
        let changed_files = checkpoint
            .as_ref()
            .map(|checkpoint| parse_optional_json(Some(&checkpoint.changed_files_json)))
            .unwrap_or(Value::Null);
        let affected_paths = affected_paths_from_json(&pending_action, &changed_files);

        Ok(RuntimeApprovalDetail {
            ok: true,
            approval,
            task,
            checkpoint,
            pending_action,
            args,
            changed_files,
            affected_paths,
        })
    }

    pub fn checkpoint_detail(&self, id: &str) -> Result<RuntimeCheckpointDetail> {
        let checkpoint = self
            .store
            .checkpoints()
            .get(id)?
            .with_context(|| format!("checkpoint not found: {}", id))?;
        let approval = match checkpoint.approval_id.as_deref() {
            Some(approval_id) => self.store.approvals().get(approval_id)?,
            None => None,
        };
        let task = match checkpoint.task_id.as_deref() {
            Some(task_id) => self.store.tasks().get(task_id)?,
            None => None,
        };
        let pending_action = parse_optional_json(checkpoint.pending_action_json.as_deref());
        let changed_files = parse_optional_json(Some(&checkpoint.changed_files_json));
        let affected_paths = affected_paths_from_json(&pending_action, &changed_files);

        Ok(RuntimeCheckpointDetail {
            ok: true,
            checkpoint,
            task,
            approval,
            pending_action,
            changed_files,
            affected_paths,
        })
    }

    pub fn list_tasks(&self, limit: usize) -> Result<Vec<TaskRecord>> {
        self.store.tasks().list(limit)
    }

    pub fn list_processes(&self, limit: usize) -> Result<Vec<ProcessRecord>> {
        self.store.processes().list(limit)
    }

    pub fn list_approvals(&self) -> Result<Vec<ApprovalRecord>> {
        self.store.approvals().list_pending()
    }

    pub fn list_tool_runs(&self, limit: usize) -> Result<Vec<ToolRunRecord>> {
        self.store.tool_runs().list(limit)
    }

    pub fn list_checkpoints(&self, limit: usize) -> Result<Vec<CheckpointRecord>> {
        self.store.checkpoints().list(limit)
    }

    pub fn list_plugins(&self) -> Result<Vec<PluginInstallRecord>> {
        self.store.plugins().list()
    }

    pub fn process_log(&self, id: &str, tail_bytes: Option<u64>) -> Result<RuntimeProcessLog> {
        let process = self
            .store
            .processes()
            .get(id)?
            .with_context(|| format!("process not found: {}", id))?;
        let path = process
            .log_path
            .with_context(|| format!("process has no log path: {}", id))?;
        let bytes = std::fs::read(&path).with_context(|| format!("failed to read {}", path))?;
        let tail = tail_bytes.unwrap_or(32 * 1024).min(512 * 1024) as usize;
        let start = bytes.len().saturating_sub(tail);
        Ok(RuntimeProcessLog {
            ok: true,
            content: String::from_utf8_lossy(&bytes[start..]).into_owned(),
        })
    }

    pub fn stop_process(&self, id: &str) -> Result<ProcessRecord> {
        let process = self
            .store
            .processes()
            .get(id)?
            .with_context(|| format!("process not found: {}", id))?;
        kill_pid(process.pid).with_context(|| format!("failed to stop pid {}", process.pid))?;
        self.store.processes().upsert(NewProcess {
            id: Some(process.id.clone()),
            task_id: process.task_id.clone(),
            pid: process.pid,
            command: process.command.clone(),
            cwd: process.cwd.clone(),
            log_path: process.log_path.clone(),
            detected_url: process.detected_url.clone(),
            status: ProcessStatus::Exited,
            health: Some("stopped".to_string()),
        })
    }

    pub fn restart_process(&self, id: &str) -> Result<ProcessRecord> {
        let process = self
            .store
            .processes()
            .get(id)?
            .with_context(|| format!("process not found: {}", id))?;
        let _ = kill_pid(process.pid);
        // Wait (bounded) for the old PID to actually exit before respawning —
        // the kill above is fire-and-forget (async signal / taskkill), so an
        // immediate respawn can race the old process for its listening port.
        // Poll until it's gone, then fail-open after 5s.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while pid_alive(process.pid) {
            if std::time::Instant::now() >= deadline {
                tracing::warn!(
                    pid = process.pid,
                    "restart_process: old pid still alive after 5s; respawning anyway"
                );
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let mut command = Command::new("sh");
        command.arg("-c").arg(&process.command);
        if let Some(cwd) = process.cwd.as_deref() {
            command.current_dir(cwd);
        }
        if let Some(path) = process.log_path.as_deref() {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .with_context(|| format!("failed to open process log {}", path))?;
            let stderr = file.try_clone()?;
            command
                .stdout(Stdio::from(file))
                .stderr(Stdio::from(stderr));
        }
        let child = command.spawn()?;
        self.store.processes().upsert(NewProcess {
            id: Some(process.id),
            task_id: process.task_id,
            pid: child.id(),
            command: process.command,
            cwd: process.cwd,
            log_path: process.log_path,
            detected_url: process.detected_url,
            status: ProcessStatus::Running,
            health: Some("restarted".to_string()),
        })
    }

    pub fn open_process(&self, id: &str) -> Result<RuntimeProcessOpen> {
        let process = self
            .store
            .processes()
            .get(id)?
            .with_context(|| format!("process not found: {}", id))?;
        let target = process
            .detected_url
            .or(process.log_path)
            .with_context(|| format!("process has no URL or log path: {}", id))?;
        crate::utils::open_file(target.clone());
        Ok(RuntimeProcessOpen { ok: true, target })
    }

    pub fn resolve_open_target(&self, target: &str) -> Result<String> {
        if let Some(process) = self.store.processes().get(target)?
            && let Some(target) = process.detected_url.or(process.log_path)
        {
            return Ok(target);
        }
        Ok(target.to_string())
    }

    pub fn ports(&self) -> Result<RuntimePorts> {
        let output = if cfg!(windows) {
            Command::new("netstat").arg("-ano").output()?
        } else {
            Command::new("sh")
                .arg("-c")
                .arg("ss -ltnp 2>/dev/null || netstat -ltnp 2>/dev/null || true")
                .output()?
        };
        Ok(RuntimePorts {
            ok: true,
            ports: String::from_utf8_lossy(&output.stdout).into_owned(),
        })
    }

    pub fn set_plugin_enabled(&self, id: &str, enabled: bool) -> Result<()> {
        self.store.plugins().set_enabled(id, enabled)?;
        super::write_plugin_lockfile()?;
        Ok(())
    }

    pub fn set_safety_mode(&self, mode: &str) -> Result<crate::app::SafetyConfig> {
        let mut config = crate::app::load_config().unwrap_or_default();
        config.safety.mode = super::SafetyMode::parse(mode)
            .ok_or_else(|| anyhow::anyhow!("unknown safety mode: {}", mode))?;
        crate::app::save_config(&config, None)?;
        Ok(config.safety)
    }

    pub fn model_info(&self, model: &str) -> Value {
        let snapshot = crate::domain::ProviderCapabilitySnapshot::from_model_id(model);
        for (key, value, confidence) in [
            (
                "supports_tools",
                snapshot.supports_tools.to_string(),
                "static",
            ),
            (
                "supports_vision",
                snapshot.supports_vision.to_string(),
                "static",
            ),
            ("reasoning", snapshot.reasoning.clone(), "static"),
            (
                "max_context_tokens",
                snapshot
                    .max_context_tokens
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "unknown".to_string()),
                "static",
            ),
        ] {
            let _ = self.store.provider_probes().upsert(NewProviderProbe {
                provider: snapshot.provider.clone(),
                model_id: snapshot.model.clone(),
                capability_key: key.to_string(),
                capability_value: value,
                confidence: confidence.to_string(),
                error: None,
            });
        }
        if let Some(profile) = crate::models::lookup_provider(&snapshot.provider) {
            for (key, value) in [
                (
                    "max_output_tokens_param",
                    format!("{:?}", profile.max_tokens_param),
                ),
                (
                    "parallel_tool_calls",
                    (!profile
                        .disable_parallel_tool_calls_for
                        .iter()
                        .any(|disabled| *disabled == snapshot.model))
                    .to_string(),
                ),
                (
                    "reasoning_parameter_shape",
                    format!("{:?}", profile.reasoning_strategy),
                ),
                (
                    "streaming_usage_available",
                    "provider_dependent".to_string(),
                ),
                ("token_usage_field_shape", "openai_compatible".to_string()),
            ] {
                let _ = self.store.provider_probes().upsert(NewProviderProbe {
                    provider: snapshot.provider.clone(),
                    model_id: snapshot.model.clone(),
                    capability_key: key.to_string(),
                    capability_value: value,
                    confidence: "static".to_string(),
                    error: None,
                });
            }
        }
        json!({
            "id": model,
            "provider": snapshot.provider,
            "model": snapshot.model,
            "supports_tools": snapshot.supports_tools,
            "supports_vision": snapshot.supports_vision,
            "reasoning": snapshot.reasoning,
            "max_context_tokens": snapshot.max_context_tokens,
        })
    }

    pub fn approval_decision(result: ApprovalReplayResult) -> Result<RuntimeApprovalDecision> {
        Ok(RuntimeApprovalDecision {
            ok: true,
            approval: result.approval,
            replayed: result.replayed,
            summary: result.summary,
        })
    }

    fn raw_hygiene_preview(&self) -> Result<RawRuntimeHygienePreview> {
        let checkpoints = self
            .store
            .checkpoints()
            .list_all(10_000)?
            .into_iter()
            .filter(|checkpoint| checkpoint.archived_at.is_none())
            .collect::<Vec<_>>();
        let test_checkpoint_ids = checkpoints
            .iter()
            .filter(|checkpoint| is_runtime_hygiene_checkpoint(checkpoint))
            .map(|checkpoint| checkpoint.id.clone())
            .collect::<HashSet<_>>();
        let approvals = self
            .store
            .approvals()
            .list_all(10_000)?
            .into_iter()
            .filter(|approval| approval.archived_at.is_none())
            .filter(|approval| is_runtime_hygiene_approval(approval, &test_checkpoint_ids))
            .collect::<Vec<_>>();
        let checkpoints = checkpoints
            .into_iter()
            .filter(is_runtime_hygiene_checkpoint)
            .collect::<Vec<_>>();

        Ok(RawRuntimeHygienePreview {
            approvals,
            checkpoints,
        })
    }
}

#[derive(Debug)]
struct RawRuntimeHygienePreview {
    approvals: Vec<ApprovalRecord>,
    checkpoints: Vec<CheckpointRecord>,
}

pub fn runtime_hygiene_reason() -> &'static str {
    "runtime hygiene: test/dev artifact"
}

pub fn parse_optional_json(raw: Option<&str>) -> Value {
    raw.and_then(|value| serde_json::from_str(value).ok())
        .unwrap_or(Value::Null)
}

pub fn affected_paths_from_json(pending_action: &Value, changed_files: &Value) -> Vec<String> {
    let mut paths = Vec::new();
    collect_path_like_strings(changed_files, &mut paths);
    collect_path_like_strings(pending_action, &mut paths);
    paths.sort();
    paths.dedup();
    paths.truncate(25);
    paths
}

fn collect_path_like_strings(value: &Value, paths: &mut Vec<String>) {
    match value {
        Value::String(value) => {
            if looks_path_like(value) {
                paths.push(value.clone());
            }
        },
        Value::Array(items) => {
            for item in items {
                collect_path_like_strings(item, paths);
            }
        },
        Value::Object(object) => {
            for (key, value) in object {
                match value {
                    Value::String(text)
                        if matches!(
                            key.as_str(),
                            "path"
                                | "file"
                                | "file_path"
                                | "target"
                                | "project_path"
                                | "snapshot_path"
                                | "cwd"
                        ) =>
                    {
                        if !text.trim().is_empty() {
                            paths.push(text.clone());
                        }
                    },
                    other => collect_path_like_strings(other, paths),
                }
            }
        },
        _ => {},
    }
}

fn looks_path_like(value: &str) -> bool {
    let trimmed = value.trim();
    trimmed.starts_with('/')
        || trimmed.starts_with("./")
        || trimmed.starts_with("../")
        || trimmed.contains(std::path::MAIN_SEPARATOR)
}

fn is_runtime_hygiene_checkpoint(checkpoint: &CheckpointRecord) -> bool {
    checkpoint.project_path.starts_with("/tmp/mermaid_")
}

fn is_runtime_hygiene_approval(
    approval: &ApprovalRecord,
    test_checkpoint_ids: &HashSet<String>,
) -> bool {
    let is_restore_replay = approval.risk_classification == "restored_action"
        && approval.proposed_action.starts_with("restore replay:");
    if !is_restore_replay {
        return false;
    }
    match approval.checkpoint_id.as_deref() {
        Some(checkpoint_id) => test_checkpoint_ids.contains(checkpoint_id),
        None => true,
    }
}

fn kill_pid(pid: u32) -> Result<()> {
    if cfg!(windows) {
        Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .status()?;
    } else {
        Command::new("kill").arg(pid.to_string()).status()?;
    }
    Ok(())
}

/// Best-effort synchronous liveness check for a pid. The runtime client is sync,
/// so it can't reuse the async `process_running` in `providers::tool::exec`.
fn pid_alive(pid: u32) -> bool {
    if cfg!(windows) {
        Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .map(|out| String::from_utf8_lossy(&out.stdout).contains(&pid.to_string()))
            .unwrap_or(false)
    } else {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_db(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mermaid_runtime_client_{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir.join("runtime.sqlite3")
    }

    #[test]
    fn pid_alive_detects_live_and_dead() {
        // Our own process is alive.
        assert!(pid_alive(std::process::id()));
        // A reaped child is dead (#6 — the restart wait polls this).
        #[cfg(unix)]
        let mut child = Command::new("true").spawn().expect("spawn true");
        #[cfg(windows)]
        let mut child = Command::new("cmd")
            .args(["/C", "exit 0"])
            .spawn()
            .expect("spawn exit");
        let pid = child.id();
        let _ = child.wait();
        std::thread::sleep(std::time::Duration::from_millis(250));
        assert!(!pid_alive(pid));
    }

    #[test]
    fn local_client_lists_tasks_from_store() {
        let path = temp_db("tasks");
        let service = RuntimeService::from_store(RuntimeStore::open(&path).expect("open store"));
        let task = service
            .store
            .tasks()
            .create(super::super::NewTask::new(
                "contract task",
                "/tmp/mermaid_runtime_client",
                "openai/test",
            ))
            .expect("task");
        let tasks = service.list_tasks(10).expect("list");
        assert_eq!(
            tasks.first().map(|item| item.id.as_str()),
            Some(task.id.as_str())
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn hygiene_preview_matches_test_artifacts_and_archive_is_idempotent() {
        let path = temp_db("hygiene");
        let store = RuntimeStore::open(&path).expect("open store");
        let checkpoint = store
            .checkpoints()
            .create(super::super::NewCheckpoint {
                id: Some("checkpoint-test".to_string()),
                task_id: None,
                project_path: "/tmp/mermaid_checkpoint_test".to_string(),
                snapshot_path: "/data/checkpoints/checkpoint-test".to_string(),
                changed_files_json: "[]".to_string(),
                pending_action_json: Some("{\"tool\":\"write_file\"}".to_string()),
                approval_id: None,
            })
            .expect("create checkpoint");
        let approval = store
            .approvals()
            .create(super::super::NewApproval {
                task_id: None,
                proposed_action: "restore replay: write_file".to_string(),
                risk_classification: "restored_action".to_string(),
                policy_decision: "ask".to_string(),
                args_summary: None,
                checkpoint_id: Some(checkpoint.id.clone()),
                pending_action_json: Some("{\"tool\":\"write_file\"}".to_string()),
            })
            .expect("create approval");
        store
            .checkpoints()
            .set_approval(&checkpoint.id, &approval.id)
            .expect("link approval");

        let service = RuntimeService::from_store(store);
        let preview = service.hygiene_preview().expect("preview");
        assert_eq!(preview.counts.approvals, 1);
        assert_eq!(preview.counts.checkpoints, 1);
        let archived = service.hygiene_archive().expect("archive");
        assert_eq!(archived.archived.total, 2);
        let archived_again = service.hygiene_archive().expect("archive again");
        assert_eq!(archived_again.archived.total, 0);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
