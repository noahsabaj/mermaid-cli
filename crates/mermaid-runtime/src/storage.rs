use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use rusqlite::types::Type;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

const SCHEMA_VERSION: i32 = 1;

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
    pub fn as_str(self) -> &'static str {
        match self {
            TaskStatus::Queued => "queued",
            TaskStatus::Running => "running",
            TaskStatus::WaitingForApproval => "waiting_for_approval",
            TaskStatus::Blocked => "blocked",
            TaskStatus::Completed => "completed",
            TaskStatus::Failed => "failed",
            TaskStatus::Cancelled => "cancelled",
        }
    }

    fn from_db(value: &str) -> std::result::Result<Self, UnknownRuntimeEnum> {
        match value {
            "queued" => Ok(TaskStatus::Queued),
            "running" => Ok(TaskStatus::Running),
            "waiting_for_approval" => Ok(TaskStatus::WaitingForApproval),
            "blocked" => Ok(TaskStatus::Blocked),
            "completed" => Ok(TaskStatus::Completed),
            "failed" => Ok(TaskStatus::Failed),
            "cancelled" => Ok(TaskStatus::Cancelled),
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
    pub fn as_str(self) -> &'static str {
        match self {
            TaskPriority::Low => "low",
            TaskPriority::Normal => "normal",
            TaskPriority::High => "high",
        }
    }

    fn from_db(value: &str) -> std::result::Result<Self, UnknownRuntimeEnum> {
        match value {
            "low" => Ok(TaskPriority::Low),
            "normal" => Ok(TaskPriority::Normal),
            "high" => Ok(TaskPriority::High),
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
    pub fn as_str(self) -> &'static str {
        match self {
            ProcessStatus::Running => "running",
            ProcessStatus::Exited => "exited",
            ProcessStatus::Unknown => "unknown",
        }
    }

    fn from_db(value: &str) -> std::result::Result<Self, UnknownRuntimeEnum> {
        match value {
            "running" => Ok(ProcessStatus::Running),
            "exited" => Ok(ProcessStatus::Exited),
            "unknown" => Ok(ProcessStatus::Unknown),
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
        }
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

/// SQLite-backed durable runtime state.
pub struct RuntimeStore {
    conn: Connection,
    path: PathBuf,
}

impl RuntimeStore {
    pub fn open_default() -> Result<Self> {
        let dir = data_dir()?;
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create Mermaid data dir {}", dir.display()))?;
        // The data dir holds the daemon control socket, pairing tokens, and
        // session/memory state. Restrict it to the owning user (0700) so no
        // other local UID can reach the socket or read the DB.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        }
        // Windows has no mode bits, so the DB (token hashes, transcripts) would
        // otherwise inherit the parent's default ACL. Lock it to the current
        // user via `icacls`. A sentinel makes this run once (first open, or
        // first open after upgrade for an already-loose dir) rather than on
        // every store open — the daemon opens the store per request. Best-effort
        // like the Unix branch: never fail the store open on an ACL hiccup.
        #[cfg(windows)]
        {
            let sentinel = dir.join(".acl-hardened");
            if !sentinel.exists()
                && let Ok(user) = std::env::var("USERNAME")
                && !user.is_empty()
            {
                let hardened = std::process::Command::new("icacls")
                    .arg(&dir)
                    .arg("/inheritance:r")
                    .arg("/grant:r")
                    .arg(format!("{user}:(OI)(CI)F"))
                    .arg("/T")
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .map(|status| status.success())
                    .unwrap_or(false);
                if hardened {
                    let _ = std::fs::write(&sentinel, b"1");
                }
            }
        }
        Self::open(dir.join("runtime.sqlite3"))
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("failed to create SQLite parent dir {}", parent.display())
            })?;
        }
        let conn = Connection::open(&path)
            .with_context(|| format!("failed to open runtime DB {}", path.display()))?;
        // The daemon, CLI, and per-turn effect tasks each open their own
        // connection (often in separate processes). Without WAL + a busy
        // timeout, a writer holding the DB makes a concurrent write fail
        // immediately with SQLITE_BUSY (lost task/tool/approval updates).
        // WAL allows concurrent readers with a single writer; busy_timeout
        // serializes writers gracefully.
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .context("failed to set SQLite busy_timeout")?;
        // `foreign_keys` is connection-scoped and can only be toggled in
        // autocommit mode, so it lives here (per connection) rather than inside
        // the now-transactional `init_schema` migration, where a PRAGMA
        // foreign_keys would be a silent no-op.
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON;",
        )
        .context("failed to set SQLite connection PRAGMAs")?;
        let store = Self { conn, path };
        store.init_schema()?;
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn sessions(&self) -> SessionsRepo<'_> {
        SessionsRepo { conn: &self.conn }
    }

    pub fn messages(&self) -> MessagesRepo<'_> {
        MessagesRepo { conn: &self.conn }
    }

    pub fn tasks(&self) -> TasksRepo<'_> {
        TasksRepo { conn: &self.conn }
    }

    pub fn tool_runs(&self) -> ToolRunsRepo<'_> {
        ToolRunsRepo { conn: &self.conn }
    }

    pub fn approvals(&self) -> ApprovalsRepo<'_> {
        ApprovalsRepo { conn: &self.conn }
    }

    pub fn processes(&self) -> ProcessesRepo<'_> {
        ProcessesRepo { conn: &self.conn }
    }

    pub fn checkpoints(&self) -> CheckpointsRepo<'_> {
        CheckpointsRepo { conn: &self.conn }
    }

    pub fn compactions(&self) -> CompactionsRepo<'_> {
        CompactionsRepo { conn: &self.conn }
    }

    pub fn plugins(&self) -> PluginsRepo<'_> {
        PluginsRepo { conn: &self.conn }
    }

    pub fn provider_probes(&self) -> ProviderProbesRepo<'_> {
        ProviderProbesRepo { conn: &self.conn }
    }

    pub fn pairing_tokens(&self) -> PairingTokensRepo<'_> {
        PairingTokensRepo { conn: &self.conn }
    }

    fn init_schema(&self) -> Result<()> {
        let conn = &self.conn;
        // Forward-compat gate: read the stored schema version BEFORE writing
        // anything. A DB written by a newer mermaid (higher `user_version`)
        // must be refused, not silently down-labeled. The old code stamped
        // `PRAGMA user_version = 1` inside the CREATE script — before this
        // check — so the guard was dead and an older binary would happily
        // operate (and corrupt) a newer DB.
        let current: i32 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        anyhow::ensure!(
            current <= SCHEMA_VERSION,
            "runtime DB schema version {} is newer than this build supports ({}); upgrade mermaid",
            current,
            SCHEMA_VERSION
        );

        // Create tables + run column migrations exactly once, even when the
        // daemon and CLI open the DB concurrently: BEGIN IMMEDIATE takes the
        // write lock up front, so a racing process blocks on `busy_timeout`
        // and, once we commit, sees the schema already in place instead of
        // double-running an ALTER and failing the open (the old check-then-
        // ALTER `ensure_column` race).
        conn.execute_batch("BEGIN IMMEDIATE;")?;
        if let Err(error) = self.migrate_within_txn() {
            let _ = conn.execute_batch("ROLLBACK;");
            return Err(error);
        }
        conn.execute_batch("COMMIT;")?;

        // Stamp the version only after a successful migration — never before
        // the gate above.
        conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))?;
        let version: i32 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        anyhow::ensure!(
            version == SCHEMA_VERSION,
            "unsupported runtime DB schema version {} (expected {})",
            version,
            SCHEMA_VERSION
        );
        Ok(())
    }

    /// Schema creation + column migrations, run inside the `init_schema`
    /// transaction. Idempotent: `CREATE TABLE IF NOT EXISTS` plus the
    /// duplicate-tolerant `ensure_column` make a re-run a no-op, so a second
    /// concurrent opener that wins the lock after us does no harm.
    fn migrate_within_txn(&self) -> Result<()> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                project_path TEXT NOT NULL,
                model_id TEXT NOT NULL,
                title TEXT,
                conversation_path TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                total_tokens INTEGER
            );

            CREATE TABLE IF NOT EXISTS messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                role TEXT NOT NULL,
                content_json TEXT NOT NULL,
                created_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_messages_session_id ON messages(session_id);

            CREATE TABLE IF NOT EXISTS tasks (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                status TEXT NOT NULL,
                priority TEXT NOT NULL,
                project_path TEXT NOT NULL,
                model_id TEXT NOT NULL,
                conversation_id TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                final_report TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_tasks_project_status
                ON tasks(project_path, status, updated_at);

            CREATE TABLE IF NOT EXISTS task_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
                kind TEXT NOT NULL,
                message TEXT NOT NULL,
                created_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_task_events_task_id
                ON task_events(task_id, id);

            CREATE TABLE IF NOT EXISTS tool_runs (
                id TEXT PRIMARY KEY,
                task_id TEXT REFERENCES tasks(id) ON DELETE SET NULL,
                turn_id TEXT,
                call_id TEXT,
                tool_name TEXT NOT NULL,
                status TEXT NOT NULL,
                args_json TEXT,
                output_json TEXT,
                started_at TEXT NOT NULL,
                finished_at TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_tool_runs_task_id ON tool_runs(task_id);

            CREATE TABLE IF NOT EXISTS approvals (
                id TEXT PRIMARY KEY,
                task_id TEXT REFERENCES tasks(id) ON DELETE SET NULL,
                proposed_action TEXT NOT NULL,
                risk_classification TEXT NOT NULL,
                policy_decision TEXT NOT NULL,
                user_decision TEXT,
                args_summary TEXT,
                checkpoint_id TEXT,
                pending_action_json TEXT,
                created_at TEXT NOT NULL,
                decided_at TEXT,
                archived_at TEXT,
                archive_reason TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_approvals_task_id ON approvals(task_id);

            CREATE TABLE IF NOT EXISTS processes (
                id TEXT PRIMARY KEY,
                task_id TEXT REFERENCES tasks(id) ON DELETE SET NULL,
                pid INTEGER NOT NULL,
                command TEXT NOT NULL,
                cwd TEXT,
                log_path TEXT,
                detected_url TEXT,
                status TEXT NOT NULL,
                health TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_processes_task_id ON processes(task_id);
            CREATE INDEX IF NOT EXISTS idx_processes_pid ON processes(pid);

            CREATE TABLE IF NOT EXISTS checkpoints (
                id TEXT PRIMARY KEY,
                task_id TEXT REFERENCES tasks(id) ON DELETE SET NULL,
                project_path TEXT NOT NULL,
                snapshot_path TEXT NOT NULL,
                changed_files_json TEXT NOT NULL,
                pending_action_json TEXT,
                approval_id TEXT REFERENCES approvals(id) ON DELETE SET NULL,
                created_at TEXT NOT NULL,
                archived_at TEXT,
                archive_reason TEXT
            );

            CREATE TABLE IF NOT EXISTS compactions (
                id TEXT PRIMARY KEY,
                task_id TEXT REFERENCES tasks(id) ON DELETE SET NULL,
                session_id TEXT,
                source_token_estimate INTEGER,
                summary_token_count INTEGER,
                preserved_turns INTEGER,
                archive_path TEXT,
                verification_status TEXT,
                created_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS provider_probes (
                provider TEXT NOT NULL,
                model_id TEXT NOT NULL,
                capability_key TEXT NOT NULL,
                capability_value TEXT NOT NULL,
                confidence TEXT NOT NULL,
                error TEXT,
                probed_at TEXT NOT NULL,
                PRIMARY KEY (provider, model_id, capability_key)
            );

            CREATE TABLE IF NOT EXISTS plugin_installs (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                source TEXT NOT NULL,
                version TEXT,
                enabled INTEGER NOT NULL DEFAULT 1,
                manifest_json TEXT NOT NULL,
                installed_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS pairing_tokens (
                id TEXT PRIMARY KEY,
                token_hash TEXT NOT NULL,
                label TEXT,
                enabled INTEGER NOT NULL DEFAULT 1,
                created_at TEXT NOT NULL,
                last_used_at TEXT,
                expires_at TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_pairing_tokens_enabled
                ON pairing_tokens(enabled, created_at);
            "#,
        )?;

        ensure_column(&self.conn, "approvals", "pending_action_json", "TEXT")?;
        ensure_column(&self.conn, "approvals", "archived_at", "TEXT")?;
        ensure_column(&self.conn, "approvals", "archive_reason", "TEXT")?;
        ensure_column(&self.conn, "checkpoints", "archived_at", "TEXT")?;
        ensure_column(&self.conn, "checkpoints", "archive_reason", "TEXT")?;
        // Pairing-token TTL. When the column is first added to an existing DB,
        // backfill live tokens with a 30-day grace window from now rather than
        // expiring them instantly on upgrade. Fresh DBs already have the column
        // (so no backfill) and only tokens minted with `--ttl-days 0` keep a
        // NULL (never-expires) value going forward.
        if ensure_column(&self.conn, "pairing_tokens", "expires_at", "TEXT")? {
            let grace = (chrono::Utc::now() + chrono::Duration::days(30)).to_rfc3339();
            self.conn.execute(
                "UPDATE pairing_tokens SET expires_at = ?1 WHERE expires_at IS NULL",
                params![grace],
            )?;
        }
        Ok(())
    }
}

pub struct TasksRepo<'a> {
    conn: &'a Connection,
}

pub struct SessionsRepo<'a> {
    conn: &'a Connection,
}

impl SessionsRepo<'_> {
    pub fn upsert(&self, new: NewSession) -> Result<SessionRecord> {
        let now = now_rfc3339();
        let id = new.id.unwrap_or_else(|| fresh_id("session"));
        self.conn.execute(
            "INSERT INTO sessions
             (id, project_path, model_id, title, conversation_path, created_at, updated_at, total_tokens)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(id) DO UPDATE SET
                project_path = excluded.project_path,
                model_id = excluded.model_id,
                title = excluded.title,
                conversation_path = excluded.conversation_path,
                updated_at = excluded.updated_at,
                total_tokens = excluded.total_tokens",
            params![
                id,
                new.project_path,
                new.model_id,
                new.title,
                new.conversation_path,
                now,
                now,
                new.total_tokens,
            ],
        )?;
        self.get(&id)?
            .context("session was upserted but could not be reloaded")
    }

    pub fn get(&self, id: &str) -> Result<Option<SessionRecord>> {
        self.conn
            .query_row(
                "SELECT id, project_path, model_id, title, conversation_path,
                        created_at, updated_at, total_tokens
                 FROM sessions WHERE id = ?1",
                [id],
                session_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn list(&self, limit: usize) -> Result<Vec<SessionRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, project_path, model_id, title, conversation_path,
                    created_at, updated_at, total_tokens
             FROM sessions ORDER BY updated_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], session_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }
}

pub struct MessagesRepo<'a> {
    conn: &'a Connection,
}

impl MessagesRepo<'_> {
    pub fn add(&self, new: NewMessage) -> Result<MessageRecord> {
        self.conn.execute(
            "INSERT INTO messages (session_id, role, content_json, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![new.session_id, new.role, new.content_json, now_rfc3339()],
        )?;
        let id = self.conn.last_insert_rowid();
        self.get(id)?
            .context("message was inserted but could not be reloaded")
    }

    pub fn get(&self, id: i64) -> Result<Option<MessageRecord>> {
        self.conn
            .query_row(
                "SELECT id, session_id, role, content_json, created_at
                 FROM messages WHERE id = ?1",
                [id],
                message_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn list_for_session(&self, session_id: &str) -> Result<Vec<MessageRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, role, content_json, created_at
             FROM messages WHERE session_id = ?1 ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([session_id], message_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }
}

impl TasksRepo<'_> {
    pub fn create(&self, new: NewTask) -> Result<TaskRecord> {
        let now = now_rfc3339();
        let record = TaskRecord {
            id: fresh_id("task"),
            title: new.title,
            status: TaskStatus::Queued,
            priority: new.priority,
            project_path: new.project_path,
            model_id: new.model_id,
            conversation_id: new.conversation_id,
            created_at: now.clone(),
            updated_at: now.clone(),
            final_report: None,
        };
        // The task row and its initial event are one logical write — commit
        // them atomically so a crash between can't leave an event-less task.
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO tasks
             (id, title, status, priority, project_path, model_id, conversation_id, created_at, updated_at, final_report)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                record.id,
                record.title,
                record.status.as_str(),
                record.priority.as_str(),
                record.project_path,
                record.model_id,
                record.conversation_id,
                record.created_at,
                record.updated_at,
                record.final_report,
            ],
        )?;
        tx.execute(
            "INSERT INTO task_events (task_id, kind, message, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![record.id, "task_created", "task created", now],
        )?;
        tx.commit()?;
        self.get(&record.id)?
            .context("task was inserted but could not be reloaded")
    }

    pub fn get(&self, id: &str) -> Result<Option<TaskRecord>> {
        self.conn
            .query_row(
                "SELECT id, title, status, priority, project_path, model_id, conversation_id,
                        created_at, updated_at, final_report
                 FROM tasks WHERE id = ?1",
                [id],
                task_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn list(&self, limit: usize) -> Result<Vec<TaskRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, status, priority, project_path, model_id, conversation_id,
                    created_at, updated_at, final_report
             FROM tasks
             ORDER BY updated_at DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], task_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn update_status(
        &self,
        id: &str,
        status: TaskStatus,
        final_report: Option<&str>,
    ) -> Result<()> {
        let now = now_rfc3339();
        // Status update + its event are one logical write.
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE tasks
             SET status = ?2, updated_at = ?3, final_report = COALESCE(?4, final_report)
             WHERE id = ?1",
            params![id, status.as_str(), now, final_report],
        )?;
        tx.execute(
            "INSERT INTO task_events (task_id, kind, message, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                id,
                "status_changed",
                format!("status changed to {status}"),
                now
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn add_event(&self, task_id: &str, kind: &str, message: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO task_events (task_id, kind, message, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![task_id, kind, message, now_rfc3339()],
        )?;
        Ok(())
    }

    pub fn events(&self, task_id: &str) -> Result<Vec<TaskTimelineEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, task_id, kind, message, created_at
             FROM task_events
             WHERE task_id = ?1
             ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([task_id], |row| {
            Ok(TaskTimelineEvent {
                id: row.get("id")?,
                task_id: row.get("task_id")?,
                kind: row.get("kind")?,
                message: row.get("message")?,
                created_at: row.get("created_at")?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }
}

pub struct ToolRunsRepo<'a> {
    conn: &'a Connection,
}

impl ToolRunsRepo<'_> {
    pub fn start(&self, new: NewToolRun) -> Result<ToolRunRecord> {
        let id = new.id.unwrap_or_else(|| fresh_id("toolrun"));
        self.conn.execute(
            "INSERT INTO tool_runs
             (id, task_id, turn_id, call_id, tool_name, status, args_json, output_json, started_at, finished_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'running', ?6, NULL, ?7, NULL)",
            params![
                id,
                new.task_id,
                new.turn_id,
                new.call_id,
                new.tool_name,
                new.args_json,
                now_rfc3339(),
            ],
        )?;
        self.get(&id)?
            .context("tool run was inserted but could not be reloaded")
    }

    pub fn finish(&self, id: &str, status: &str, output_json: Option<&str>) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE tool_runs
             SET status = ?2, output_json = ?3, finished_at = ?4
             WHERE id = ?1",
            params![id, status, output_json, now_rfc3339()],
        )?;
        anyhow::ensure!(changed > 0, "tool run not found: {}", id);
        Ok(())
    }

    pub fn get(&self, id: &str) -> Result<Option<ToolRunRecord>> {
        self.conn
            .query_row(
                "SELECT id, task_id, turn_id, call_id, tool_name, status, args_json,
                        output_json, started_at, finished_at
                 FROM tool_runs WHERE id = ?1",
                [id],
                tool_run_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn list(&self, limit: usize) -> Result<Vec<ToolRunRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, task_id, turn_id, call_id, tool_name, status, args_json,
                    output_json, started_at, finished_at
             FROM tool_runs ORDER BY started_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], tool_run_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }
}

pub struct ApprovalsRepo<'a> {
    conn: &'a Connection,
}

impl ApprovalsRepo<'_> {
    pub fn create(&self, new: NewApproval) -> Result<ApprovalRecord> {
        let record = ApprovalRecord {
            id: fresh_id("approval"),
            task_id: new.task_id,
            proposed_action: new.proposed_action,
            risk_classification: new.risk_classification,
            policy_decision: new.policy_decision,
            user_decision: None,
            args_summary: new.args_summary,
            checkpoint_id: new.checkpoint_id,
            pending_action_json: new.pending_action_json,
            created_at: now_rfc3339(),
            decided_at: None,
            archived_at: None,
            archive_reason: None,
        };
        self.conn.execute(
            "INSERT INTO approvals
             (id, task_id, proposed_action, risk_classification, policy_decision, user_decision,
              args_summary, checkpoint_id, pending_action_json, created_at, decided_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                record.id,
                record.task_id,
                record.proposed_action,
                record.risk_classification,
                record.policy_decision,
                record.user_decision,
                record.args_summary,
                record.checkpoint_id,
                record.pending_action_json,
                record.created_at,
                record.decided_at,
            ],
        )?;
        self.get(&record.id)?
            .context("approval was inserted but could not be reloaded")
    }

    pub fn get(&self, id: &str) -> Result<Option<ApprovalRecord>> {
        self.conn
            .query_row(
                "SELECT id, task_id, proposed_action, risk_classification, policy_decision,
                        user_decision, args_summary, checkpoint_id, pending_action_json,
                        created_at, decided_at, archived_at, archive_reason
                 FROM approvals WHERE id = ?1",
                [id],
                approval_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn decide(&self, id: &str, user_decision: &str) -> Result<()> {
        // Single-shot decision: only an undecided, un-archived approval can be
        // decided, so a denied approval cannot be resurrected as "approved".
        // `approval::approve_and_replay` runs the (un-rollback-able) replay
        // effect *before* calling `decide`, so the "approved" mark lands only
        // after the action ran: a crash mid-replay leaves the row undecided and
        // safely re-runnable, never "approved but never applied" (#62). Mirrors
        // the `archive` `WHERE archived_at IS NULL` idempotency pattern below.
        let changed = self.conn.execute(
            "UPDATE approvals
             SET user_decision = ?2, decided_at = ?3
             WHERE id = ?1 AND user_decision IS NULL AND archived_at IS NULL",
            params![id, user_decision, now_rfc3339()],
        )?;
        anyhow::ensure!(
            changed > 0,
            "approval {} cannot be decided (already decided, archived, or not found)",
            id
        );
        Ok(())
    }

    pub fn list_pending(&self) -> Result<Vec<ApprovalRecord>> {
        self.list_pending_with_archived(false)
    }

    pub fn list_pending_all(&self) -> Result<Vec<ApprovalRecord>> {
        self.list_pending_with_archived(true)
    }

    pub fn list_all(&self, limit: usize) -> Result<Vec<ApprovalRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, task_id, proposed_action, risk_classification, policy_decision,
                    user_decision, args_summary, checkpoint_id, pending_action_json,
                    created_at, decided_at, archived_at, archive_reason
             FROM approvals
             ORDER BY created_at DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], approval_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    fn list_pending_with_archived(&self, include_archived: bool) -> Result<Vec<ApprovalRecord>> {
        let archived_filter = if include_archived {
            ""
        } else {
            " AND archived_at IS NULL"
        };
        let mut stmt = self.conn.prepare(&format!(
            "SELECT id, task_id, proposed_action, risk_classification, policy_decision,
                    user_decision, args_summary, checkpoint_id, pending_action_json,
                    created_at, decided_at, archived_at, archive_reason
             FROM approvals
             WHERE user_decision IS NULL{archived_filter}
             ORDER BY created_at DESC"
        ))?;
        let rows = stmt.query_map([], approval_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn archive(&self, ids: &[String], reason: &str) -> Result<usize> {
        let archived_at = now_rfc3339();
        let mut changed = 0;
        for id in ids {
            changed += self.conn.execute(
                "UPDATE approvals
                 SET archived_at = COALESCE(archived_at, ?2),
                     archive_reason = COALESCE(archive_reason, ?3)
                 WHERE id = ?1 AND archived_at IS NULL",
                params![id, archived_at, reason],
            )?;
        }
        Ok(changed)
    }

    pub fn count_archived(&self) -> Result<usize> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM approvals WHERE archived_at IS NOT NULL",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map(|count| count as usize)
            .map_err(Into::into)
    }
}

pub struct ProcessesRepo<'a> {
    conn: &'a Connection,
}

impl ProcessesRepo<'_> {
    pub fn upsert(&self, new: NewProcess) -> Result<ProcessRecord> {
        let now = now_rfc3339();
        let id = new.id.unwrap_or_else(|| fresh_id("process"));
        self.conn.execute(
            "INSERT INTO processes
             (id, task_id, pid, command, cwd, log_path, detected_url, status, health, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(id) DO UPDATE SET
                task_id = excluded.task_id,
                pid = excluded.pid,
                command = excluded.command,
                cwd = excluded.cwd,
                log_path = excluded.log_path,
                detected_url = excluded.detected_url,
                status = excluded.status,
                health = excluded.health,
                updated_at = excluded.updated_at",
            params![
                id,
                new.task_id,
                new.pid,
                new.command,
                new.cwd,
                new.log_path,
                new.detected_url,
                new.status.as_str(),
                new.health,
                now,
                now,
            ],
        )?;
        self.get(&id)?
            .context("process was upserted but could not be reloaded")
    }

    pub fn get(&self, id: &str) -> Result<Option<ProcessRecord>> {
        self.conn
            .query_row(
                "SELECT id, task_id, pid, command, cwd, log_path, detected_url, status, health,
                        created_at, updated_at
                 FROM processes WHERE id = ?1",
                [id],
                process_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn list(&self, limit: usize) -> Result<Vec<ProcessRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, task_id, pid, command, cwd, log_path, detected_url, status, health,
                    created_at, updated_at
             FROM processes
             ORDER BY updated_at DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], process_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }
}

pub struct CheckpointsRepo<'a> {
    conn: &'a Connection,
}

impl CheckpointsRepo<'_> {
    pub fn create(&self, new: NewCheckpoint) -> Result<CheckpointRecord> {
        let id = new.id.unwrap_or_else(|| fresh_id("checkpoint"));
        self.conn.execute(
            "INSERT INTO checkpoints
             (id, task_id, project_path, snapshot_path, changed_files_json,
              pending_action_json, approval_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                id,
                new.task_id,
                new.project_path,
                new.snapshot_path,
                new.changed_files_json,
                new.pending_action_json,
                new.approval_id,
                now_rfc3339(),
            ],
        )?;
        self.get(&id)?
            .context("checkpoint was inserted but could not be reloaded")
    }

    pub fn get(&self, id: &str) -> Result<Option<CheckpointRecord>> {
        self.conn
            .query_row(
                "SELECT id, task_id, project_path, snapshot_path, changed_files_json,
                        pending_action_json, approval_id, created_at, archived_at, archive_reason
                 FROM checkpoints WHERE id = ?1",
                [id],
                checkpoint_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn set_approval(&self, id: &str, approval_id: &str) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE checkpoints SET approval_id = ?2 WHERE id = ?1",
            params![id, approval_id],
        )?;
        anyhow::ensure!(changed > 0, "checkpoint not found: {}", id);
        Ok(())
    }

    pub fn list(&self, limit: usize) -> Result<Vec<CheckpointRecord>> {
        self.list_with_archived(limit, false)
    }

    pub fn list_all(&self, limit: usize) -> Result<Vec<CheckpointRecord>> {
        self.list_with_archived(limit, true)
    }

    fn list_with_archived(
        &self,
        limit: usize,
        include_archived: bool,
    ) -> Result<Vec<CheckpointRecord>> {
        let archived_filter = if include_archived {
            ""
        } else {
            "WHERE archived_at IS NULL"
        };
        let mut stmt = self.conn.prepare(&format!(
            "SELECT id, task_id, project_path, snapshot_path, changed_files_json,
                    pending_action_json, approval_id, created_at, archived_at, archive_reason
             FROM checkpoints {archived_filter} ORDER BY created_at DESC LIMIT ?1"
        ))?;
        let rows = stmt.query_map([limit as i64], checkpoint_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn archive(&self, ids: &[String], reason: &str) -> Result<usize> {
        let archived_at = now_rfc3339();
        let mut changed = 0;
        for id in ids {
            changed += self.conn.execute(
                "UPDATE checkpoints
                 SET archived_at = COALESCE(archived_at, ?2),
                     archive_reason = COALESCE(archive_reason, ?3)
                 WHERE id = ?1 AND archived_at IS NULL",
                params![id, archived_at, reason],
            )?;
        }
        Ok(changed)
    }

    pub fn count_archived(&self) -> Result<usize> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM checkpoints WHERE archived_at IS NOT NULL",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map(|count| count as usize)
            .map_err(Into::into)
    }
}

pub struct CompactionsRepo<'a> {
    conn: &'a Connection,
}

impl CompactionsRepo<'_> {
    pub fn create(&self, new: NewCompaction) -> Result<CompactionRecord> {
        let id = new.id.unwrap_or_else(|| fresh_id("compaction"));
        self.conn.execute(
            "INSERT INTO compactions
             (id, task_id, session_id, source_token_estimate, summary_token_count,
              preserved_turns, archive_path, verification_status, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(id) DO UPDATE SET
                task_id = excluded.task_id,
                session_id = excluded.session_id,
                source_token_estimate = excluded.source_token_estimate,
                summary_token_count = excluded.summary_token_count,
                preserved_turns = excluded.preserved_turns,
                archive_path = excluded.archive_path,
                verification_status = excluded.verification_status",
            params![
                id,
                new.task_id,
                new.session_id,
                new.source_token_estimate,
                new.summary_token_count,
                new.preserved_turns,
                new.archive_path,
                new.verification_status,
                now_rfc3339(),
            ],
        )?;
        self.get(&id)?
            .context("compaction was inserted but could not be reloaded")
    }

    pub fn get(&self, id: &str) -> Result<Option<CompactionRecord>> {
        self.conn
            .query_row(
                "SELECT id, task_id, session_id, source_token_estimate, summary_token_count,
                        preserved_turns, archive_path, verification_status, created_at
                 FROM compactions WHERE id = ?1",
                [id],
                compaction_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn list(&self, limit: usize) -> Result<Vec<CompactionRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, task_id, session_id, source_token_estimate, summary_token_count,
                    preserved_turns, archive_path, verification_status, created_at
             FROM compactions ORDER BY created_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], compaction_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }
}

pub struct PluginsRepo<'a> {
    conn: &'a Connection,
}

impl PluginsRepo<'_> {
    pub fn install(&self, new: NewPluginInstall) -> Result<PluginInstallRecord> {
        let now = now_rfc3339();
        let id = new.id.unwrap_or_else(|| fresh_id("plugin"));
        self.conn.execute(
            "INSERT INTO plugin_installs
             (id, name, source, version, enabled, manifest_json, installed_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(id) DO UPDATE SET
                name = excluded.name,
                source = excluded.source,
                version = excluded.version,
                enabled = excluded.enabled,
                manifest_json = excluded.manifest_json,
                updated_at = excluded.updated_at",
            params![
                id,
                new.name,
                new.source,
                new.version,
                if new.enabled { 1 } else { 0 },
                new.manifest_json,
                now,
                now,
            ],
        )?;
        self.get(&id)?
            .context("plugin install was inserted but could not be reloaded")
    }

    pub fn get(&self, id: &str) -> Result<Option<PluginInstallRecord>> {
        self.conn
            .query_row(
                "SELECT id, name, source, version, enabled, manifest_json, installed_at, updated_at
                 FROM plugin_installs WHERE id = ?1",
                [id],
                plugin_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn list(&self) -> Result<Vec<PluginInstallRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, source, version, enabled, manifest_json, installed_at, updated_at
             FROM plugin_installs ORDER BY name ASC",
        )?;
        let rows = stmt.query_map([], plugin_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn set_enabled(&self, id: &str, enabled: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE plugin_installs SET enabled = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, if enabled { 1 } else { 0 }, now_rfc3339()],
        )?;
        Ok(())
    }
}

pub struct ProviderProbesRepo<'a> {
    conn: &'a Connection,
}

impl ProviderProbesRepo<'_> {
    pub fn upsert(&self, new: NewProviderProbe) -> Result<ProviderProbeRecord> {
        let now = now_rfc3339();
        let provider = new.provider;
        let model_id = new.model_id;
        let capability_key = new.capability_key;
        self.conn.execute(
            "INSERT INTO provider_probes
             (provider, model_id, capability_key, capability_value, confidence, error, probed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(provider, model_id, capability_key) DO UPDATE SET
                capability_value = excluded.capability_value,
                confidence = excluded.confidence,
                error = excluded.error,
                probed_at = excluded.probed_at",
            params![
                &provider,
                &model_id,
                &capability_key,
                new.capability_value,
                new.confidence,
                new.error,
                now,
            ],
        )?;
        self.get(&provider, &model_id, &capability_key)?
            .context("provider probe was inserted but could not be reloaded")
    }

    pub fn get(
        &self,
        provider: &str,
        model_id: &str,
        capability_key: &str,
    ) -> Result<Option<ProviderProbeRecord>> {
        self.conn
            .query_row(
                "SELECT provider, model_id, capability_key, capability_value, confidence, error, probed_at
                 FROM provider_probes
                 WHERE provider = ?1 AND model_id = ?2 AND capability_key = ?3",
                params![provider, model_id, capability_key],
                provider_probe_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn list(
        &self,
        provider: Option<&str>,
        model_id: Option<&str>,
    ) -> Result<Vec<ProviderProbeRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT provider, model_id, capability_key, capability_value, confidence, error, probed_at
             FROM provider_probes ORDER BY provider ASC, model_id ASC, capability_key ASC",
        )?;
        let rows = stmt.query_map([], provider_probe_from_row)?;
        let mut out = Vec::new();
        for row in rows {
            let probe = row?;
            if provider.is_some_and(|p| probe.provider != p) {
                continue;
            }
            if model_id.is_some_and(|m| probe.model_id != m) {
                continue;
            }
            out.push(probe);
        }
        Ok(out)
    }
}

pub struct PairingTokensRepo<'a> {
    conn: &'a Connection,
}

impl PairingTokensRepo<'_> {
    pub fn create(
        &self,
        token_hash: &str,
        label: Option<&str>,
        expires_at: Option<&str>,
    ) -> Result<PairingTokenRecord> {
        let id = fresh_id("pairing");
        self.conn.execute(
            "INSERT INTO pairing_tokens
                 (id, token_hash, label, enabled, created_at, last_used_at, expires_at)
             VALUES (?1, ?2, ?3, 1, ?4, NULL, ?5)",
            params![id, token_hash, label, now_rfc3339(), expires_at],
        )?;
        self.get(&id)?
            .context("pairing token was inserted but could not be reloaded")
    }

    pub fn get(&self, id: &str) -> Result<Option<PairingTokenRecord>> {
        self.conn
            .query_row(
                "SELECT id, token_hash, label, enabled, created_at, last_used_at, expires_at
                 FROM pairing_tokens WHERE id = ?1",
                [id],
                pairing_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    /// Look up an enabled, unexpired pairing token by hash.
    ///
    /// The hash is **not** matched in SQL (`WHERE token_hash = ?`) — that is a
    /// DB-level equality on the secret and a theoretical timing channel.
    /// Instead we fetch the enabled, unexpired candidates (neither predicate is
    /// secret) and compare each hash in constant time. The candidate count is
    /// tiny and not secret. All candidates are scanned without early exit so the
    /// timing doesn't reveal which (if any) token matched.
    pub fn verify_token(&self, token_hash: &str) -> Result<Option<PairingTokenRecord>> {
        // Expiry is evaluated in Rust as a parsed instant (see `is_expired`),
        // not via a SQL `expires_at > ?` string compare. The skipped-because-
        // expired branch is on non-secret data; the hash itself is still matched
        // in constant time over every non-expired candidate with no early exit.
        let now = chrono::Utc::now();
        let mut stmt = self.conn.prepare(
            "SELECT id, token_hash, label, enabled, created_at, last_used_at, expires_at
             FROM pairing_tokens
             WHERE enabled = 1",
        )?;
        let candidates = stmt
            .query_map([], pairing_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut found = None;
        for record in candidates {
            if is_expired(record.expires_at.as_deref(), now) {
                continue;
            }
            if ct_eq(record.token_hash.as_bytes(), token_hash.as_bytes()) {
                found = Some(record);
            }
        }
        Ok(found)
    }

    pub fn list(&self) -> Result<Vec<PairingTokenRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, token_hash, label, enabled, created_at, last_used_at, expires_at
             FROM pairing_tokens ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([], pairing_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Like [`list`](Self::list), but with `token_hash` blanked. Use for any
    /// surface that crosses a trust boundary — e.g. the daemon snapshot served
    /// over the local socket to same-UID processes. The hash is
    /// secret-equivalent (it's all `verify_token` compares against) and must
    /// not leave the store.
    pub fn list_redacted(&self) -> Result<Vec<PairingTokenRecord>> {
        Ok(self
            .list()?
            .into_iter()
            .map(|mut record| {
                record.token_hash = String::new();
                record
            })
            .collect())
    }

    pub fn mark_used(&self, id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE pairing_tokens SET last_used_at = ?2 WHERE id = ?1 AND enabled = 1",
            params![id, now_rfc3339()],
        )?;
        Ok(())
    }

    /// Revoke a token by disabling it. Returns `true` if a live token was
    /// revoked, `false` if it was already disabled or unknown.
    pub fn revoke(&self, id: &str) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE pairing_tokens SET enabled = 0 WHERE id = ?1 AND enabled = 1",
            params![id],
        )?;
        Ok(changed > 0)
    }
}

/// Add `column` to `table` if it is missing. Returns `true` iff the column was
/// just created (so the caller can run a one-time backfill).
///
/// SQL identifiers cannot be bound as `?` parameters, so `table`/`column`/
/// `definition` are interpolated. All call sites pass compile-time constants
/// today; the validation below makes that a hard invariant rather than a latent
/// injection footgun if a future caller ever threads in dynamic input.
fn ensure_column(conn: &Connection, table: &str, column: &str, definition: &str) -> Result<bool> {
    fn is_sql_identifier(s: &str) -> bool {
        let mut chars = s.chars();
        matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
            && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    }
    const ALLOWED_DEFINITIONS: &[&str] = &["TEXT", "INTEGER", "REAL", "BLOB"];
    anyhow::ensure!(
        is_sql_identifier(table),
        "invalid table identifier: {table}"
    );
    anyhow::ensure!(
        is_sql_identifier(column),
        "invalid column identifier: {column}"
    );
    anyhow::ensure!(
        ALLOWED_DEFINITIONS.contains(&definition),
        "unsupported column definition: {definition}"
    );

    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == column {
            return Ok(false);
        }
    }
    // Tolerate a concurrent opener that added the column between our
    // `table_info` check and this ALTER. SQLite reports that as a "duplicate
    // column name" schema error (not SQLITE_BUSY, so `busy_timeout` can't retry
    // it); treat it as already-present rather than failing the whole store open.
    match conn.execute(
        &format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"),
        [],
    ) {
        Ok(_) => Ok(true),
        Err(error) if error.to_string().contains("duplicate column") => Ok(false),
        Err(error) => Err(error.into()),
    }
}

pub fn data_dir() -> Result<PathBuf> {
    if let Some(proj_dirs) = ProjectDirs::from("", "", "mermaid") {
        return Ok(proj_dirs.data_dir().to_path_buf());
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .context("could not determine home directory")?;
    Ok(PathBuf::from(home).join(".local/share/mermaid"))
}

fn session_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRecord> {
    Ok(SessionRecord {
        id: row.get("id")?,
        project_path: row.get("project_path")?,
        model_id: row.get("model_id")?,
        title: row.get("title")?,
        conversation_path: row.get("conversation_path")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        total_tokens: row.get("total_tokens")?,
    })
}

fn message_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<MessageRecord> {
    Ok(MessageRecord {
        id: row.get("id")?,
        session_id: row.get("session_id")?,
        role: row.get("role")?,
        content_json: row.get("content_json")?,
        created_at: row.get("created_at")?,
    })
}

fn task_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskRecord> {
    let status_raw: String = row.get("status")?;
    let priority_raw: String = row.get("priority")?;
    Ok(TaskRecord {
        id: row.get("id")?,
        title: row.get("title")?,
        status: TaskStatus::from_db(&status_raw)
            .map_err(|e| enum_from_sql_error("status", status_raw, e))?,
        priority: TaskPriority::from_db(&priority_raw)
            .map_err(|e| enum_from_sql_error("priority", priority_raw, e))?,
        project_path: row.get("project_path")?,
        model_id: row.get("model_id")?,
        conversation_id: row.get("conversation_id")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        final_report: row.get("final_report")?,
    })
}

fn process_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ProcessRecord> {
    let status_raw: String = row.get("status")?;
    let pid: i64 = row.get("pid")?;
    Ok(ProcessRecord {
        id: row.get("id")?,
        task_id: row.get("task_id")?,
        pid: pid as u32,
        command: row.get("command")?,
        cwd: row.get("cwd")?,
        log_path: row.get("log_path")?,
        detected_url: row.get("detected_url")?,
        status: ProcessStatus::from_db(&status_raw)
            .map_err(|e| enum_from_sql_error("status", status_raw, e))?,
        health: row.get("health")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

fn tool_run_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ToolRunRecord> {
    Ok(ToolRunRecord {
        id: row.get("id")?,
        task_id: row.get("task_id")?,
        turn_id: row.get("turn_id")?,
        call_id: row.get("call_id")?,
        tool_name: row.get("tool_name")?,
        status: row.get("status")?,
        args_json: row.get("args_json")?,
        output_json: row.get("output_json")?,
        started_at: row.get("started_at")?,
        finished_at: row.get("finished_at")?,
    })
}

fn approval_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ApprovalRecord> {
    Ok(ApprovalRecord {
        id: row.get("id")?,
        task_id: row.get("task_id")?,
        proposed_action: row.get("proposed_action")?,
        risk_classification: row.get("risk_classification")?,
        policy_decision: row.get("policy_decision")?,
        user_decision: row.get("user_decision")?,
        args_summary: row.get("args_summary")?,
        checkpoint_id: row.get("checkpoint_id")?,
        pending_action_json: row.get("pending_action_json")?,
        created_at: row.get("created_at")?,
        decided_at: row.get("decided_at")?,
        archived_at: row.get("archived_at")?,
        archive_reason: row.get("archive_reason")?,
    })
}

fn checkpoint_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CheckpointRecord> {
    Ok(CheckpointRecord {
        id: row.get("id")?,
        task_id: row.get("task_id")?,
        project_path: row.get("project_path")?,
        snapshot_path: row.get("snapshot_path")?,
        changed_files_json: row.get("changed_files_json")?,
        pending_action_json: row.get("pending_action_json")?,
        approval_id: row.get("approval_id")?,
        created_at: row.get("created_at")?,
        archived_at: row.get("archived_at")?,
        archive_reason: row.get("archive_reason")?,
    })
}

fn compaction_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CompactionRecord> {
    Ok(CompactionRecord {
        id: row.get("id")?,
        task_id: row.get("task_id")?,
        session_id: row.get("session_id")?,
        source_token_estimate: row.get("source_token_estimate")?,
        summary_token_count: row.get("summary_token_count")?,
        preserved_turns: row.get("preserved_turns")?,
        archive_path: row.get("archive_path")?,
        verification_status: row.get("verification_status")?,
        created_at: row.get("created_at")?,
    })
}

fn plugin_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PluginInstallRecord> {
    let enabled: i64 = row.get("enabled")?;
    Ok(PluginInstallRecord {
        id: row.get("id")?,
        name: row.get("name")?,
        source: row.get("source")?,
        version: row.get("version")?,
        enabled: enabled != 0,
        manifest_json: row.get("manifest_json")?,
        installed_at: row.get("installed_at")?,
        updated_at: row.get("updated_at")?,
    })
}

fn provider_probe_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ProviderProbeRecord> {
    Ok(ProviderProbeRecord {
        provider: row.get("provider")?,
        model_id: row.get("model_id")?,
        capability_key: row.get("capability_key")?,
        capability_value: row.get("capability_value")?,
        confidence: row.get("confidence")?,
        error: row.get("error")?,
        probed_at: row.get("probed_at")?,
    })
}

fn pairing_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PairingTokenRecord> {
    let enabled: i64 = row.get("enabled")?;
    Ok(PairingTokenRecord {
        id: row.get("id")?,
        token_hash: row.get("token_hash")?,
        label: row.get("label")?,
        enabled: enabled != 0,
        created_at: row.get("created_at")?,
        last_used_at: row.get("last_used_at")?,
        expires_at: row.get("expires_at")?,
    })
}

fn enum_from_sql_error(
    column: &'static str,
    value: String,
    source: UnknownRuntimeEnum,
) -> rusqlite::Error {
    let _ = value;
    rusqlite::Error::FromSqlConversionFailure(column_index(column), Type::Text, Box::new(source))
}

fn column_index(column: &str) -> usize {
    match column {
        "status" => 2,
        "priority" => 3,
        _ => 0,
    }
}

#[derive(Debug)]
struct UnknownRuntimeEnum {
    kind: &'static str,
    value: String,
}

impl UnknownRuntimeEnum {
    fn new(kind: &'static str, value: &str) -> Self {
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

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Constant-time byte-slice equality. Unlike `==` (or a SQL `=`), it never
/// short-circuits on the first differing byte, so it leaks no timing signal
/// about how much of a secret matched. Lengths are compared first; the length
/// of a token hash is fixed and not secret.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Whether a pairing token's `expires_at` is in the past relative to `now`.
///
/// `None` (SQL `NULL`) means "never expires" — the documented `--ttl-days 0`
/// opt-out. A present-but-unparseable value fails closed (treated as expired).
/// Expiry is compared as a parsed instant rather than via a SQL `expires_at > ?`
/// string compare, which only orders correctly while every stored value is the
/// canonical `now_rfc3339()` shape (#64).
fn is_expired(expires_at: Option<&str>, now: chrono::DateTime<chrono::Utc>) -> bool {
    match expires_at {
        None => false,
        Some(raw) => match chrono::DateTime::parse_from_rfc3339(raw) {
            Ok(dt) => dt <= now,
            Err(_) => true,
        },
    }
}

fn fresh_id(prefix: &str) -> String {
    // In-process monotonic counter: two ids minted in the same nanosecond (a
    // coarse clock, or a clock stepping backward) can never be equal, so the
    // `ON CONFLICT(id) DO UPDATE` upserts can't silently overwrite an unrelated
    // row (#61). A per-process random salt removes the clock dependence so ids
    // minted across a daemon restart don't collide either (getrandom is already
    // a dependency — see `daemon.rs`).
    static SEQ: AtomicU64 = AtomicU64::new(0);
    static SALT: OnceLock<u64> = OnceLock::new();
    let salt = *SALT.get_or_init(|| {
        let mut bytes = [0u8; 8];
        // The monotonic counter alone still guarantees in-process uniqueness if
        // the RNG ever fails, so a best-effort fill is fine here.
        let _ = getrandom::fill(&mut bytes);
        u64::from_le_bytes(bytes)
    });
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{nanos:x}-{salt:x}-{seq:x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_enables_wal_and_busy_timeout() {
        // H19: every connection must use WAL so daemon/CLI/effect writers
        // don't hit a hard SQLITE_BUSY.
        let path = temp_db("wal_check");
        let store = RuntimeStore::open(&path).expect("open");
        let mode: String = store
            .conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .expect("journal_mode pragma");
        assert_eq!(mode.to_lowercase(), "wal");
    }

    fn temp_db(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mermaid_runtime_store_{}", name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir.join("runtime.sqlite3")
    }

    #[test]
    fn initializes_runtime_schema() {
        let path = temp_db("schema");
        let store = RuntimeStore::open(&path).expect("open store");
        assert_eq!(store.path(), path.as_path());
        let version: i32 = store
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn rejects_newer_schema_version() {
        // Forward-compat gate: a DB stamped with a newer schema must be
        // refused, not silently down-labeled and operated on (RC-5).
        let path = temp_db("newer_schema");
        {
            let store = RuntimeStore::open(&path).expect("first open");
            store
                .conn
                .execute_batch(&format!("PRAGMA user_version = {};", SCHEMA_VERSION + 1))
                .expect("bump version");
        }
        // `RuntimeStore` isn't `Debug`, so match rather than `expect_err`.
        let err = match RuntimeStore::open(&path) {
            Ok(_) => panic!("must refuse a newer DB"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("newer than this build"),
            "unexpected error: {err}"
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn init_schema_is_idempotent_across_opens() {
        // Re-opening the same DB re-runs `init_schema`; it must succeed (the
        // create script and `ensure_column` are idempotent) and keep the
        // version stamped.
        let path = temp_db("idempotent_schema");
        let _ = RuntimeStore::open(&path).expect("first open");
        let store = RuntimeStore::open(&path).expect("second open must succeed");
        let version: i32 = store
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn task_create_commits_task_and_event_atomically() {
        // The task row and its `task_created` event commit in one transaction.
        let path = temp_db("task_txn");
        let store = RuntimeStore::open(&path).expect("open");
        let task = store
            .tasks()
            .create(NewTask::new("do a thing", "/repo", "anthropic/claude"))
            .expect("create task");
        let events = store.tasks().events(&task.id).expect("events");
        assert!(
            events.iter().any(|e| e.kind == "task_created"),
            "the task_created event must commit with the task row"
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn task_lifecycle_round_trips() {
        let path = temp_db("task");
        let store = RuntimeStore::open(&path).expect("open store");
        let session = store
            .sessions()
            .upsert(NewSession {
                id: Some("session-1".to_string()),
                project_path: "/repo".to_string(),
                model_id: "anthropic/claude".to_string(),
                title: Some("Run tests".to_string()),
                conversation_path: Some("/repo/.mermaid/session.json".to_string()),
                total_tokens: Some(42),
            })
            .expect("upsert session");
        assert_eq!(session.id, "session-1");
        let message = store
            .messages()
            .add(NewMessage {
                session_id: session.id.clone(),
                role: "user".to_string(),
                content_json: "{\"text\":\"hi\"}".to_string(),
            })
            .expect("add message");
        assert_eq!(message.role, "user");
        assert_eq!(
            store
                .messages()
                .list_for_session(&session.id)
                .unwrap()
                .len(),
            1
        );

        let mut new = NewTask::new("Run tests", "/repo", "anthropic/claude");
        new.priority = TaskPriority::High;
        let task = store.tasks().create(new).expect("create task");

        assert_eq!(task.status, TaskStatus::Queued);
        assert_eq!(task.priority, TaskPriority::High);

        store
            .tasks()
            .update_status(&task.id, TaskStatus::Completed, Some("tests passed"))
            .expect("update task");
        let loaded = store.tasks().get(&task.id).unwrap().unwrap();
        assert_eq!(loaded.status, TaskStatus::Completed);
        assert_eq!(loaded.final_report.as_deref(), Some("tests passed"));

        let events = store.tasks().events(&task.id).expect("events");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, "task_created");
        assert_eq!(events[1].kind, "status_changed");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn approval_and_process_records_round_trip() {
        let path = temp_db("approval_process");
        let store = RuntimeStore::open(&path).expect("open store");
        let task = store
            .tasks()
            .create(NewTask::new("Edit files", "/repo", "openai/gpt-5.2"))
            .expect("create task");

        let approval = store
            .approvals()
            .create(NewApproval {
                task_id: Some(task.id.clone()),
                proposed_action: "write_file src/lib.rs".to_string(),
                risk_classification: "file_mutation".to_string(),
                policy_decision: "ask".to_string(),
                args_summary: Some("src/lib.rs".to_string()),
                checkpoint_id: Some("checkpoint-1".to_string()),
                pending_action_json: Some(
                    "{\"tool\":\"write_file\",\"args\":{\"path\":\"src/lib.rs\"}}".to_string(),
                ),
            })
            .expect("create approval");
        store
            .approvals()
            .decide(&approval.id, "approved")
            .expect("decide approval");
        let approval = store.approvals().get(&approval.id).unwrap().unwrap();
        assert_eq!(approval.user_decision.as_deref(), Some("approved"));
        assert!(approval.pending_action_json.is_some());

        let tool_run = store
            .tool_runs()
            .start(NewToolRun {
                id: Some("toolrun-1".to_string()),
                task_id: Some(task.id.clone()),
                turn_id: Some("turn-1".to_string()),
                call_id: Some("call-1".to_string()),
                tool_name: "write_file".to_string(),
                args_json: Some("{\"path\":\"src/lib.rs\"}".to_string()),
            })
            .expect("start tool run");
        assert_eq!(tool_run.status, "running");
        store
            .tool_runs()
            .finish("toolrun-1", "success", Some("{\"summary\":\"ok\"}"))
            .expect("finish tool run");
        let tool_run = store.tool_runs().get("toolrun-1").unwrap().unwrap();
        assert_eq!(tool_run.status, "success");
        assert!(tool_run.finished_at.is_some());

        let process = store
            .processes()
            .upsert(NewProcess {
                id: Some("proc-1".to_string()),
                task_id: Some(task.id),
                pid: 123,
                command: "npm run dev".to_string(),
                cwd: Some("/repo".to_string()),
                log_path: Some("/tmp/mermaid.log".to_string()),
                detected_url: Some("http://127.0.0.1:5173".to_string()),
                status: ProcessStatus::Running,
                health: Some("ready".to_string()),
            })
            .expect("upsert process");
        assert_eq!(process.status, ProcessStatus::Running);
        assert_eq!(store.processes().list(10).unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn approval_decide_is_single_shot() {
        let path = temp_db("approval_decide_guard");
        let store = RuntimeStore::open(&path).expect("open store");
        let make = |action: &str| {
            store
                .approvals()
                .create(NewApproval {
                    task_id: None,
                    proposed_action: action.to_string(),
                    risk_classification: "file_mutation".to_string(),
                    policy_decision: "ask".to_string(),
                    args_summary: None,
                    checkpoint_id: None,
                    pending_action_json: None,
                })
                .expect("create approval")
        };

        // A second decision on an already-decided approval is rejected — this
        // is what stops a stored action from being replayed N times.
        let a = make("write_file a");
        store
            .approvals()
            .decide(&a.id, "approved")
            .expect("first decide");
        assert!(
            store.approvals().decide(&a.id, "approved").is_err(),
            "re-approving an approved approval must be rejected"
        );

        // A denied approval cannot be resurrected as approved.
        let b = make("write_file b");
        store.approvals().decide(&b.id, "denied").expect("deny");
        assert!(
            store.approvals().decide(&b.id, "approved").is_err(),
            "a denied approval must not be re-decidable as approved"
        );
        let reloaded = store.approvals().get(&b.id).unwrap().unwrap();
        assert_eq!(reloaded.user_decision.as_deref(), Some("denied"));

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn archived_approvals_and_checkpoints_are_hidden_from_visible_lists() {
        let path = temp_db("archive_visibility");
        let store = RuntimeStore::open(&path).expect("open store");

        let approval = store
            .approvals()
            .create(NewApproval {
                task_id: None,
                proposed_action: "restore replay: write_file".to_string(),
                risk_classification: "restored_action".to_string(),
                policy_decision: "ask".to_string(),
                args_summary: None,
                checkpoint_id: Some("checkpoint-1".to_string()),
                pending_action_json: Some("{\"tool\":\"write_file\"}".to_string()),
            })
            .expect("create approval");
        let checkpoint = store
            .checkpoints()
            .create(NewCheckpoint {
                id: Some("checkpoint-1".to_string()),
                task_id: None,
                project_path: "/tmp/mermaid_checkpoint_test".to_string(),
                snapshot_path: "/data/checkpoints/checkpoint-1".to_string(),
                changed_files_json: "[]".to_string(),
                pending_action_json: Some("{\"tool\":\"write_file\"}".to_string()),
                approval_id: Some(approval.id.clone()),
            })
            .expect("create checkpoint");

        assert_eq!(store.approvals().list_pending().unwrap().len(), 1);
        assert_eq!(store.approvals().list_pending_all().unwrap().len(), 1);
        assert_eq!(store.approvals().list_all(10).unwrap().len(), 1);
        assert_eq!(store.checkpoints().list(10).unwrap().len(), 1);
        assert_eq!(store.checkpoints().list_all(10).unwrap().len(), 1);

        assert_eq!(
            store
                .approvals()
                .archive(std::slice::from_ref(&approval.id), "runtime hygiene")
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .checkpoints()
                .archive(std::slice::from_ref(&checkpoint.id), "runtime hygiene")
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .approvals()
                .archive(std::slice::from_ref(&approval.id), "runtime hygiene")
                .unwrap(),
            0
        );
        assert_eq!(store.approvals().list_pending().unwrap().len(), 0);
        assert_eq!(store.approvals().list_pending_all().unwrap().len(), 1);
        assert_eq!(store.approvals().list_all(10).unwrap().len(), 1);
        assert_eq!(store.approvals().count_archived().unwrap(), 1);
        assert_eq!(store.checkpoints().list(10).unwrap().len(), 0);
        assert_eq!(store.checkpoints().list_all(10).unwrap().len(), 1);
        assert_eq!(store.checkpoints().count_archived().unwrap(), 1);

        let archived = store.approvals().get(&approval.id).unwrap().unwrap();
        assert!(archived.archived_at.is_some());
        assert_eq!(archived.archive_reason.as_deref(), Some("runtime hygiene"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn checkpoint_compaction_plugin_probe_and_pairing_round_trip() {
        let path = temp_db("everything_else");
        let store = RuntimeStore::open(&path).expect("open store");

        let checkpoint = store
            .checkpoints()
            .create(NewCheckpoint {
                id: Some("checkpoint-1".to_string()),
                task_id: None,
                project_path: "/repo".to_string(),
                snapshot_path: "/data/checkpoints/checkpoint-1".to_string(),
                changed_files_json: "[\"src/lib.rs\"]".to_string(),
                pending_action_json: Some("{\"tool\":\"write_file\"}".to_string()),
                approval_id: None,
            })
            .expect("create checkpoint");
        assert_eq!(checkpoint.id, "checkpoint-1");
        assert_eq!(store.checkpoints().list(10).unwrap().len(), 1);

        let compaction = store
            .compactions()
            .create(NewCompaction {
                id: Some("compaction-1".to_string()),
                task_id: None,
                session_id: Some("session-1".to_string()),
                source_token_estimate: Some(10_000),
                summary_token_count: Some(800),
                preserved_turns: Some(6),
                archive_path: Some(".mermaid/compactions/session-1/compaction-1.json".to_string()),
                verification_status: Some("verified".to_string()),
            })
            .expect("create compaction");
        assert_eq!(compaction.summary_token_count, Some(800));
        assert_eq!(store.compactions().list(10).unwrap().len(), 1);

        let plugin = store
            .plugins()
            .install(NewPluginInstall {
                id: Some("plugin-1".to_string()),
                name: "example".to_string(),
                source: "local".to_string(),
                version: Some("0.1.0".to_string()),
                enabled: true,
                manifest_json: "{\"name\":\"example\"}".to_string(),
            })
            .expect("install plugin");
        assert!(plugin.enabled);
        store.plugins().set_enabled("plugin-1", false).unwrap();
        assert!(!store.plugins().get("plugin-1").unwrap().unwrap().enabled);

        let probe = store
            .provider_probes()
            .upsert(NewProviderProbe {
                provider: "cerebras".to_string(),
                model_id: "gpt-oss-120b".to_string(),
                capability_key: "parallel_tool_calls".to_string(),
                capability_value: "false".to_string(),
                confidence: "static".to_string(),
                error: None,
            })
            .expect("probe");
        assert_eq!(probe.confidence, "static");
        assert_eq!(
            store
                .provider_probes()
                .list(Some("cerebras"), Some("gpt-oss-120b"))
                .unwrap()
                .len(),
            1
        );

        let pairing = store
            .pairing_tokens()
            .create("hash", Some("phone"), None)
            .expect("pairing");
        store.pairing_tokens().mark_used(&pairing.id).unwrap();
        assert!(
            store
                .pairing_tokens()
                .get(&pairing.id)
                .unwrap()
                .unwrap()
                .last_used_at
                .is_some()
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn pairing_token_expiry_and_revoke() {
        let path = temp_db("pairing_ttl");
        let store = RuntimeStore::open(&path).expect("open store");
        let tokens = store.pairing_tokens();

        // A never-expiring token verifies.
        let live = tokens
            .create("live_hash", Some("a"), None)
            .expect("create live");
        assert!(tokens.verify_token("live_hash").unwrap().is_some());

        // A future expiry still verifies; a past expiry does not.
        let future = (chrono::Utc::now() + chrono::Duration::days(1)).to_rfc3339();
        tokens
            .create("future_hash", None, Some(&future))
            .expect("create future");
        assert!(tokens.verify_token("future_hash").unwrap().is_some());

        let past = (chrono::Utc::now() - chrono::Duration::days(1)).to_rfc3339();
        tokens
            .create("past_hash", None, Some(&past))
            .expect("create past");
        assert!(
            tokens.verify_token("past_hash").unwrap().is_none(),
            "an expired token must not verify"
        );

        // A future expiry rendered with a non-UTC offset still verifies, even
        // though its RFC3339 string sorts lexically *before* `now_rfc3339()` —
        // this would wrongly read as expired under the old SQL string compare (#64).
        let skewed = (chrono::Utc::now() + chrono::Duration::hours(1))
            .with_timezone(&chrono::FixedOffset::west_opt(3 * 3600).unwrap())
            .to_rfc3339();
        tokens
            .create("skew_hash", None, Some(&skewed))
            .expect("create skewed");
        assert!(
            tokens.verify_token("skew_hash").unwrap().is_some(),
            "a future token in a non-UTC offset must verify (parsed-instant compare)"
        );

        // A present-but-unparseable expiry fails closed (treated as expired).
        tokens
            .create("garbage_hash", None, Some("not-a-timestamp"))
            .expect("create garbage");
        assert!(
            tokens.verify_token("garbage_hash").unwrap().is_none(),
            "an unparseable expiry must fail closed"
        );

        // Revoking disables the token.
        assert!(tokens.revoke(&live.id).unwrap());
        assert!(tokens.verify_token("live_hash").unwrap().is_none());
        assert!(
            !tokens.revoke(&live.id).unwrap(),
            "double revoke is a no-op"
        );

        // A non-matching hash never verifies.
        assert!(tokens.verify_token("nope").unwrap().is_none());

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn ct_eq_matches_only_identical_bytes() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
        assert!(!ct_eq(b"", b"x"));
        assert!(ct_eq(b"", b""));
    }

    #[test]
    fn fresh_id_is_collision_free_in_tight_loop() {
        // The #61 stress: ids minted back-to-back (same nanosecond on a coarse
        // clock) must all be distinct and keep the `prefix-` shape.
        let mut seen = std::collections::HashSet::new();
        for _ in 0..10_000 {
            let id = fresh_id("process");
            assert!(id.starts_with("process-"), "id must keep prefix: {id}");
            assert!(seen.insert(id), "fresh_id produced a duplicate");
        }
    }

    #[test]
    fn ensure_column_rejects_non_identifier() {
        let path = temp_db("ensure_col");
        let store = RuntimeStore::open(&path).expect("open store");
        assert!(ensure_column(&store.conn, "approvals; DROP", "x", "TEXT").is_err());
        assert!(ensure_column(&store.conn, "approvals", "x-y", "TEXT").is_err());
        assert!(ensure_column(&store.conn, "approvals", "x", "TEXT; DROP").is_err());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
