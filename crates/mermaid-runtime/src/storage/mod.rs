use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, params};

// Bumped to 5 for the additive `tasks.prompt` column (the daemon scheduler
// executes queued tasks later, so the full prompt must be persisted at enqueue
// time — `title` is truncated at 80 chars). Additive, but the bump lets a DB
// already at v4 re-run the migration once to pick it up. The bump is
// load-bearing alongside the F17 early-return in `init_schema`: a DB at an
// older version still runs the migration (the idempotent baseline plus any
// per-version step dispatched by `migrate_within_txn`) exactly once, while an
// already-current DB skips the write lock entirely.
//
// History: v2 added the additive `tasks.owner_kind` column (F18/RC-E); v3 added
// the F75 covering indexes; v4 added the `outcomes` table.

use std::sync::{Mutex, PoisonError};

/// The one store handle this process shares. Before this existed every call
/// site ran `RuntimeStore::open_default()` -- ~40 sites, each paying the
/// open + ACL check + migration probe per call and racing the others on
/// first-open migration. One handle per process removes the churn; SQLite's
/// WAL already serializes actual writes, so the mutex adds no meaningful
/// contention (every op here is a short read or insert).
///
/// The store is deliberately MULTI-PROCESS: a live `mermaid` session and a
/// running `mermaidd` share the database by design (WAL + the owner-kind
/// column keep them out of each other's rows -- see `OWNER_KIND_DAEMON`).
/// This helper unifies access WITHIN a process, not across processes.
static SHARED_STORE: Mutex<Option<RuntimeStore>> = Mutex::new(None);

/// Run one operation against the process-wide shared store, opening it on
/// first use. An open failure is returned and NOT cached, so a transient
/// failure (the data dir's drive dropping off the bus) self-heals on the
/// next call. An operation error evicts the cached handle for the same
/// reason: before this helper, every call re-opened fresh and a dead
/// connection could not outlive its call -- eviction preserves that
/// self-healing at the cost of one re-open after a genuine error.
///
/// # Errors
///
/// Whatever `RuntimeStore::open_default` or the operation itself returns.
pub fn with_shared_store<T>(op: impl FnOnce(&RuntimeStore) -> Result<T>) -> Result<T> {
    let mut guard = SHARED_STORE.lock().unwrap_or_else(PoisonError::into_inner);
    let store = match guard.as_mut() {
        Some(store) => store,
        None => guard.insert(RuntimeStore::open_default()?),
    };
    let result = op(store);
    if result.is_err() {
        *guard = None;
    }
    result
}

pub mod repos;
pub mod rows;

pub use mermaid_model::records::*;
pub use repos::*;
pub use rows::*;

// Bumped to 5 for the additive `tasks.prompt` column (the daemon scheduler
// executes queued tasks later, so the full prompt must be persisted at enqueue
// time — `title` is truncated at 80 chars). Additive, but the bump lets a DB
// already at v4 re-run the migration once to pick it up. The bump is
// load-bearing alongside the F17 early-return in `init_schema`: a DB at an
// older version still runs the migration (the idempotent baseline plus any
// per-version step dispatched by `migrate_within_txn`) exactly once, while an
// already-current DB skips the write lock entirely.
//
// History: v2 added the additive `tasks.owner_kind` column (F18/RC-E); v3 added
// the F75 covering indexes; v4 added the `outcomes` table.
pub(crate) const SCHEMA_VERSION: i32 = 7;

/// Windows ACL hardening for the data directory, and the repair path for
/// machines an earlier version locked out of their own database.
///
/// # What went wrong
///
/// The original hardening was one command:
///
/// ```text
/// icacls <dir> /inheritance:r /grant:r <user>:(OI)(CI)F /T
/// ```
///
/// `(OI)(CI)` are *inheritance* flags: they describe what the children of a
/// container inherit. `/T` applies the ACE string verbatim to every existing
/// item underneath, and on a leaf file those flags grant nothing — so
/// `/inheritance:r` stripped the file's inherited ACE and the replacement put
/// nothing back. Measured on a fresh directory:
///
/// ```text
/// <dir>            ACEs=1 [FullControl]     <- correct
/// <dir>\db.sqlite3 ACEs=0 []                <- locked out
/// <dir>\sub        ACEs=1 [FullControl]     <- correct
/// <dir>\sub\x.txt  ACEs=0 []                <- locked out
/// ```
///
/// SQLite then returns `SQLITE_CANTOPEN` (14) forever, and the sentinel — written
/// on `icacls` exit 0, which it earned — stopped the block ever running again.
///
/// # What replaces it
///
/// Harden the **directory only** and let Windows do the propagating. Removing
/// inheritance on the parent recomputes every child's inherited ACEs from the
/// new parent DACL, so existing files end up with exactly the intended access
/// and subdirectories keep their `(OI)(CI)` flags, which is what makes files
/// created *later* inherit correctly too.
///
/// The obvious-looking alternative — a `/T` pass granting plain `<user>:F`,
/// then re-flagging the directory — was measured and rejected: it leaves every
/// subdirectory with an unflagged ACE, reintroducing the same bug one level
/// down for every file created afterwards.
#[cfg(windows)]
mod windows_acl {
    use std::path::Path;
    use std::process::{Command, Stdio};

    fn icacls(args: &[&std::ffi::OsStr]) -> bool {
        Command::new("icacls")
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    /// Restrict `dir` to the current user, and only `dir`. Children follow by
    /// inheritance. Best-effort: a machine with no `USERNAME`, or an `icacls`
    /// that fails, leaves the directory as it was rather than failing the open.
    pub(super) fn harden_data_dir(dir: &Path) -> bool {
        let Ok(user) = std::env::var("USERNAME") else {
            return false;
        };
        if user.is_empty() {
            return false;
        }
        icacls(&[
            dir.as_os_str(),
            "/inheritance:r".as_ref(),
            "/grant:r".as_ref(),
            format!("{user}:(OI)(CI)F").as_ref(),
        ])
    }

    /// Give the current user access back to a single file whose DACL came out
    /// empty. An owner always retains `WRITE_DAC`, so this succeeds on exactly
    /// the machines the bug created and fails harmlessly everywhere else.
    ///
    /// `/grant` and not `/grant:r`: this is a repair, and it must add the
    /// missing ACE without discarding whatever else is legitimately there.
    pub(super) fn restore_owner_access(path: &Path) -> bool {
        let Ok(user) = std::env::var("USERNAME") else {
            return false;
        };
        if user.is_empty() {
            return false;
        }
        icacls(&[
            path.as_os_str(),
            "/grant".as_ref(),
            format!("{user}:(F)").as_ref(),
        ])
    }

    /// Can SQLite actually open this path? The only question the sentinel is
    /// allowed to be written on the answer to.
    pub(super) fn sqlite_opens(path: &Path) -> bool {
        rusqlite::Connection::open(path).is_ok()
    }
}

/// Open the connection, repairing a Windows ACL lockout once before giving up.
///
/// Scoped to `SQLITE_CANTOPEN` on a file that exists: any other failure, or a
/// missing file, means something the ACL cannot explain and must surface
/// unchanged.
#[cfg(windows)]
fn open_connection(path: &Path) -> Result<Connection> {
    let err = match Connection::open(path) {
        Ok(conn) => return Ok(conn),
        Err(err) => err,
    };
    let cannot_open = matches!(
        err,
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ErrorCode::CannotOpen,
                ..
            },
            _
        )
    );
    if !cannot_open || !path.is_file() || !windows_acl::restore_owner_access(path) {
        return Err(err).with_context(|| format!("failed to open runtime DB {}", path.display()));
    }
    tracing::warn!(
        path = %path.display(),
        "runtime DB was unreadable (an earlier Mermaid left its ACL empty); \
         restored owner access and retried"
    );
    Connection::open(path).with_context(|| {
        format!(
            "failed to open runtime DB {} even after restoring owner access",
            path.display()
        )
    })
}

#[cfg(not(windows))]
fn open_connection(path: &Path) -> Result<Connection> {
    Connection::open(path).with_context(|| format!("failed to open runtime DB {}", path.display()))
}

/// SQLite-backed durable runtime state.
pub struct RuntimeStore {
    conn: Connection,
    path: PathBuf,
}

/// The additive schema baseline: every table and index, in `IF NOT EXISTS`
/// form so a re-run is a no-op. Column additions that a `CREATE TABLE IF NOT
/// EXISTS` cannot apply to an existing table live in `migrate_within_txn`'s
/// `ensure_column` calls, and so do the indexes over those columns.
const BASELINE_SCHEMA: &str = r#"
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
        final_report TEXT,
        owner_kind TEXT
    );
    CREATE INDEX IF NOT EXISTS idx_tasks_project_status
        ON tasks(project_path, status, updated_at);
    -- `idx_tasks_status_owner` is NOT here. It indexes `owner_kind`,
    -- which the `ensure_column` below adds, and on a pre-v2 DB the
    -- `CREATE TABLE IF NOT EXISTS` above is a no-op against a table
    -- that has no such column. See the ordered block after the
    -- `ensure_column` calls.

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
    -- F75: `list_pending` scans `user_decision IS NULL ORDER BY
    -- created_at`. A partial index over only the pending rows stays tiny
    -- and serves both the filter and the ordering.
    CREATE INDEX IF NOT EXISTS idx_approvals_pending
        ON approvals(created_at)
        WHERE user_decision IS NULL;

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
        archive_reason TEXT,
        session_id TEXT,
        message_index INTEGER
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

    CREATE TABLE IF NOT EXISTS outcomes (
        id TEXT PRIMARY KEY,
        task_id TEXT REFERENCES tasks(id) ON DELETE SET NULL,
        tool_run_id TEXT REFERENCES tool_runs(id) ON DELETE SET NULL,
        kind TEXT NOT NULL,
        label TEXT NOT NULL,
        reward REAL,
        source TEXT NOT NULL,
        detail_json TEXT,
        created_at TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_outcomes_task_id ON outcomes(task_id);
    CREATE INDEX IF NOT EXISTS idx_outcomes_kind ON outcomes(kind, created_at);
"#;

impl RuntimeStore {
    /// Open the store at its default location under the app data dir,
    /// creating and (best-effort) locking down that dir first.
    ///
    /// # Errors
    ///
    /// Resolving the data dir, creating it, and everything [`Self::open`]
    /// reports. Tightening permissions is best-effort on both platforms and
    /// never fails the open, so an `Ok` store is not proof the data dir is
    /// owner-only.
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
            let db = dir.join("runtime.sqlite3");
            // The sentinel is written only after the DB is confirmed openable,
            // NOT on `icacls` exit 0. Those are different claims, and the gap
            // between them is what shipped: the old command exited 0 having
            // done exactly what it was told, and what it was told left the
            // database unreadable. See `windows_acl` for the mechanism.
            if !sentinel.exists()
                && windows_acl::harden_data_dir(&dir)
                && windows_acl::sqlite_opens(&db)
            {
                let _ = std::fs::write(&sentinel, b"1");
            }
        }
        Self::open(dir.join("runtime.sqlite3"))
    }

    /// Open (creating if needed) the SQLite store at `path`, then apply the
    /// connection pragmas and run the schema migration.
    ///
    /// # Errors
    ///
    /// Creating the parent directory, opening the database — a corrupt file or
    /// a directory the process cannot write — setting the connection pragmas,
    /// and the schema migration. A concurrent opener is not among them: WAL
    /// plus the busy timeout is exactly what keeps a second process from
    /// failing here with `SQLITE_BUSY`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("failed to create SQLite parent dir {}", parent.display())
            })?;
        }
        let conn = open_connection(&path)?;
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

    pub fn outcomes(&self) -> OutcomesRepo<'_> {
        OutcomesRepo { conn: &self.conn }
    }

    /// Every project directory this store has ever indexed a session or a
    /// task for. The daemon walks these on start to rebuild the session index
    /// from what is on disk.
    ///
    /// # Errors
    ///
    /// Errors if the statement fails to prepare or run.
    pub fn known_project_paths(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT project_path FROM sessions UNION SELECT project_path FROM tasks")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Recover state stranded by a previous daemon's crash/stop (#120, #118).
    /// A `Running` task's worker died with the daemon, so it can never finish —
    /// mark it `failed` with an event. An approval left in the transient
    /// `approving` claim state (a replay that crashed mid-effect, #118) is reset
    /// to undecided so it reappears as pending and stays re-runnable. Call once
    /// on daemon startup, before serving. Returns `(tasks_reset, claims_released)`.
    ///
    /// F18 (RC-E): only **daemon-owned** running tasks are reset. The store is
    /// shared with interactive `mermaid` CLI runs; their tasks are created with a
    /// `NULL` `owner_kind` and are LEFT RUNNING here, so a live CLI session isn't
    /// wrongly flipped to `failed` (with a spurious "interrupted" event) just
    /// because the daemon restarted. The daemon tags the tasks it runs in-process
    /// via [`NewTask::daemon_owned`].
    /// # Errors
    ///
    /// Taking the `BEGIN IMMEDIATE` write lock — which waits out a concurrent
    /// writer for `busy_timeout` before giving up — and any statement in the
    /// pass. Every failure rolls the transaction back, so recovery is
    /// all-or-nothing: no task is left flipped to `failed` without its
    /// `interrupted` event, and no claim is released without the tasks beside
    /// it. The caller may simply run it again.
    pub fn reconcile_after_restart(&self) -> Result<(usize, usize)> {
        let now = now_rfc3339();
        // Take the write lock up front with BEGIN IMMEDIATE rather than a DEFERRED
        // transaction that SELECTs and then upgrades to a write on the first
        // UPDATE: SQLite fails a read→write lock upgrade with SQLITE_BUSY
        // *immediately* (busy_timeout does not retry upgrades), so a CLI holding
        // the write lock at daemon startup would abort recovery. IMMEDIATE instead
        // waits on busy_timeout for the lock (#F21). Mirrors `init_schema`.
        self.conn.execute_batch("BEGIN IMMEDIATE;")?;
        let result = (|| -> Result<(usize, usize)> {
            let running: Vec<String> = {
                let mut stmt = self
                    .conn
                    .prepare("SELECT id FROM tasks WHERE status = 'running' AND owner_kind = ?1")?;
                let ids = stmt.query_map([OWNER_KIND_DAEMON], |row| row.get::<_, String>(0))?;
                ids.collect::<rusqlite::Result<Vec<_>>>()?
            };
            for id in &running {
                self.conn.execute(
                    "UPDATE tasks SET status = 'failed', updated_at = ?2 WHERE id = ?1",
                    params![id, now],
                )?;
                self.conn.execute(
                    "INSERT INTO task_events (task_id, kind, message, created_at)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![
                        id,
                        "interrupted",
                        "task was running when the daemon restarted; marked failed",
                        now
                    ],
                )?;
            }
            let claims_released = self.conn.execute(
                "UPDATE approvals SET user_decision = NULL WHERE user_decision = 'approving'",
                [],
            )?;
            Ok((running.len(), claims_released))
        })();
        match result {
            Ok(v) => {
                self.conn.execute_batch("COMMIT;")?;
                Ok(v)
            },
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK;");
                Err(e)
            },
        }
    }

    /// Best-effort retention GC (#130, F22/RC-F): prune archived
    /// approvals/checkpoints, the events of long-finished tasks, terminal tasks,
    /// and the high-churn / old rows of the remaining tables, all older than
    /// `retention_days`. The append-only `outcomes` reward table — the
    /// self-improving-loop training corpus — is pruned on its own, longer
    /// `outcomes_retention_days` window so a large training history survives the
    /// shorter task/session window. Deletes only archived, finished, or
    /// terminal-and-old rows — **active data is never touched** (a running task,
    /// a still-open tool run, a live process, or a recently-updated session all
    /// survive). Returns the number of rows removed.
    /// # Errors
    ///
    /// Opening the transaction and any `DELETE` in it. The whole pass is one
    /// transaction, so a failure prunes nothing and the returned count is
    /// never partial. Having nothing to prune is `Ok(0)`.
    pub fn gc(&self, retention_days: i64, outcomes_retention_days: i64) -> Result<u64> {
        let now = chrono::Utc::now();
        let cutoff = (now - chrono::Duration::days(retention_days)).to_rfc3339();
        let outcomes_cutoff = (now - chrono::Duration::days(outcomes_retention_days)).to_rfc3339();
        let tx = self.conn.unchecked_transaction()?;
        let mut removed = 0u64;
        removed += tx.execute(
            "DELETE FROM approvals WHERE archived_at IS NOT NULL AND archived_at < ?1",
            params![cutoff],
        )? as u64;
        removed += tx.execute(
            "DELETE FROM checkpoints WHERE archived_at IS NOT NULL AND archived_at < ?1",
            params![cutoff],
        )? as u64;
        removed += tx.execute(
            "DELETE FROM task_events
             WHERE created_at < ?1
               AND task_id IN (
                   SELECT id FROM tasks
                   WHERE status IN ('completed', 'failed', 'cancelled') AND updated_at < ?1
               )",
            params![cutoff],
        )? as u64;
        // F22 (RC-F): the high-churn growers. `tool_runs` is the fastest — one row
        // per tool call — so prune FINISHED runs past the window (a still-running
        // run has a NULL `finished_at` and is kept).
        removed += tx.execute(
            "DELETE FROM tool_runs WHERE finished_at IS NOT NULL AND finished_at < ?1",
            params![cutoff],
        )? as u64;
        // Exited processes past the window (a live `running`/`unknown` process is
        // kept so the dashboard and `stop`/`restart` still see it).
        removed += tx.execute(
            "DELETE FROM processes WHERE status = 'exited' AND updated_at < ?1",
            params![cutoff],
        )? as u64;
        // Old compaction history — immutable bookkeeping rows, safe to drop once
        // past the window.
        removed += tx.execute(
            "DELETE FROM compactions WHERE created_at < ?1",
            params![cutoff],
        )? as u64;
        // Sessions untouched for the whole window are treated as finished.
        // Their transcripts live in the project's conversation files, not
        // here, so pruning the index row is the whole job.
        removed += tx.execute(
            "DELETE FROM sessions WHERE updated_at < ?1",
            params![cutoff],
        )? as u64;
        // The append-only `outcomes` reward table is the training corpus for the
        // self-improving loop, so it is pruned on its own, deliberately longer
        // window. Prune it BEFORE the terminal-tasks delete below: an outcome's
        // `task_id` is `ON DELETE SET NULL`, so a task pruned on the shorter
        // window nulls the link on any still-retained outcome — the denormalized
        // `detail_json` (captured at task-terminal time) preserves the training
        // context regardless.
        removed += tx.execute(
            "DELETE FROM outcomes WHERE created_at < ?1",
            params![outcomes_cutoff],
        )? as u64;
        // Terminal tasks past the window — the #148 durable queue would otherwise
        // keep every finished task (with its full `prompt`) forever. `task_events`
        // is `ON DELETE CASCADE`, so a pruned task's events go with it (the
        // explicit task_events prune above already cleared most). A queued /
        // running / waiting task is never terminal, so live work survives.
        removed += tx.execute(
            "DELETE FROM tasks
             WHERE status IN ('completed', 'failed', 'cancelled') AND updated_at < ?1",
            params![cutoff],
        )? as u64;
        tx.commit()?;
        Ok(removed)
    }

    pub(crate) fn init_schema(&self) -> Result<()> {
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
            "runtime DB schema version {current} is newer than this build supports ({SCHEMA_VERSION}); upgrade mermaid"
        );

        // F17 (RC-E): the overwhelmingly common case is an already-current DB.
        // The daemon opens a fresh store per request, and the old code ran
        // `BEGIN IMMEDIATE` (the write lock) + the full migration + an
        // unconditional `PRAGMA user_version` write on EVERY open — so even
        // read-only requests serialized on a single writer and grew the WAL. Once
        // the stored version already matches, the schema is in place and there is
        // nothing to migrate or stamp: return before taking any write lock so
        // concurrent readers never contend. The newer-than-supported gate above
        // still runs first, so a newer DB is refused, not skipped.
        if current == SCHEMA_VERSION {
            return Ok(());
        }

        // Older (or fresh, version 0) DB only past this point.
        // Create tables + run column migrations exactly once, even when the
        // daemon and CLI open the DB concurrently: BEGIN IMMEDIATE takes the
        // write lock up front, so a racing process blocks on `busy_timeout`
        // and, once we commit, sees the schema already in place instead of
        // double-running an ALTER and failing the open (the old check-then-
        // ALTER `ensure_column` race).
        conn.execute_batch("BEGIN IMMEDIATE;")?;
        if let Err(error) = self.migrate_within_txn(current) {
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
            "unsupported runtime DB schema version {version} (expected {SCHEMA_VERSION})"
        );
        Ok(())
    }

    /// Schema creation + column migrations, run inside the `init_schema`
    /// transaction for a DB upgrading from `from_version`. Idempotent:
    /// `CREATE TABLE IF NOT EXISTS` plus the duplicate-tolerant `ensure_column`
    /// make a re-run a no-op, so a second concurrent opener that wins the lock
    /// after us does no harm.
    pub(crate) fn migrate_within_txn(&self, from_version: i32) -> Result<()> {
        self.conn.execute_batch(BASELINE_SCHEMA)?;

        ensure_column(&self.conn, "approvals", "pending_action_json", "TEXT")?;
        ensure_column(&self.conn, "approvals", "archived_at", "TEXT")?;
        ensure_column(&self.conn, "approvals", "archive_reason", "TEXT")?;
        ensure_column(&self.conn, "checkpoints", "archived_at", "TEXT")?;
        ensure_column(&self.conn, "checkpoints", "archive_reason", "TEXT")?;
        // v6: conversation anchoring for rewind/fork. Nullable + no backfill —
        // pre-existing checkpoints simply have no anchor and are excluded from
        // fork notices.
        ensure_column(&self.conn, "checkpoints", "session_id", "TEXT")?;
        ensure_column(&self.conn, "checkpoints", "message_index", "INTEGER")?;
        // Index AFTER the ensure_columns: on an upgraded DB the columns only
        // exist once the lines above ran (fresh DBs have them from CREATE).
        self.conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_checkpoints_session
                 ON checkpoints(session_id, message_index);",
        )?;
        // F18 (RC-E): task ownership. Nullable + no backfill — existing rows stay
        // `NULL` (treated as un-owned, so reconcile leaves them alone), and only
        // tasks the daemon explicitly marks `daemon` are reset on restart.
        ensure_column(&self.conn, "tasks", "owner_kind", "TEXT")?;
        // v5: full prompt for scheduler-executed tasks. Nullable — only tasks
        // enqueued for deferred daemon execution set it; the claim query treats
        // a NULL prompt as "metadata-only task, never claim".
        ensure_column(&self.conn, "tasks", "prompt", "TEXT")?;
        // F75: `reconcile_after_restart` filters `status = 'running' AND
        // owner_kind = ?`, which the (project_path, ...) index cannot serve
        // (wrong leading column). This covering index does.
        //
        // AFTER the `ensure_column` above, for the same reason
        // `idx_checkpoints_session` sits after its columns. It used to live in
        // the baseline batch, which made every v1 DB unopenable: `CREATE TABLE
        // IF NOT EXISTS tasks` is a no-op when the table already exists, so on
        // a pre-v2 DB the index was created against a table with no
        // `owner_kind` and failed with "no such column". `IF NOT EXISTS` does
        // not help — it guards the index NAME, and SQLite still parses the
        // column list. The failure landed inside the migration transaction, so
        // it rolled back and `user_version` was never stamped, and the next
        // open failed identically. Forever.
        self.conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_tasks_status_owner
                 ON tasks(status, owner_kind);",
        )?;
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

        // F76: structured per-version migration dispatch. Everything above is the
        // idempotent ADDITIVE baseline (`CREATE ... IF NOT EXISTS` + `ensure_column`),
        // always safe to re-run. This loop is the home for FUTURE NON-ADDITIVE
        // steps — dropping/renaming/transforming a column, rebuilding a table —
        // that the baseline cannot express: each target version's step runs once,
        // only when upgrading PAST it, inside this same transaction. Today every
        // shipped step is additive, so the arms are documented (near-)no-ops, but a
        // future v4 now has an ordered, versioned place to live instead of
        // overloading `IF NOT EXISTS`.
        for target in (from_version + 1)..=SCHEMA_VERSION {
            match target {
                // v2 added `tasks.owner_kind` — additive, applied by the baseline.
                2 => {},
                // v3: F75 covering indexes — additive, created by the baseline
                // above; this call is the concrete template for the first real
                // non-additive change.
                3 => self.migrate_to_v3()?,
                // v4: additive `outcomes` table — created by the idempotent
                // baseline above; this arm is its versioned home if a
                // non-additive change to that schema is ever needed.
                4 => self.migrate_to_v4()?,
                // v5: additive `tasks.prompt` column — applied by `ensure_column`
                // in the baseline above.
                5 => self.migrate_to_v5()?,
                // v6: additive `checkpoints.session_id`/`message_index` columns
                // + covering index — applied by the idempotent baseline above.
                6 => {},
                // v7: the first genuinely NON-additive step — see below.
                7 => self.migrate_to_v7()?,
                // A future v8+ adds its non-additive step here.
                _ => {},
            }
        }
        Ok(())
    }

    /// Non-additive migration steps introduced at schema v3. Today v3 only adds
    /// covering indexes (additive — applied by the idempotent baseline in
    /// [`Self::migrate_within_txn`]), so this is intentionally a no-op. It exists
    /// as the concrete template for the first real non-additive change: a step
    /// that, for example, drops or transforms a column, which
    /// `CREATE ... IF NOT EXISTS` and `ensure_column` cannot express. Runs inside
    /// the `init_schema` transaction, exactly once, when a DB upgrades past v2.
    pub(crate) fn migrate_to_v3(&self) -> Result<()> {
        Ok(())
    }

    /// Non-additive migration steps introduced at schema v4. Today v4 only adds
    /// the additive `outcomes` table (applied by the idempotent baseline in
    /// [`Self::migrate_within_txn`]), so this is intentionally a no-op — the
    /// versioned home for a future non-additive change to the outcomes schema.
    pub(crate) fn migrate_to_v4(&self) -> Result<()> {
        Ok(())
    }

    /// Non-additive migration steps introduced at schema v5. Today v5 only adds
    /// the additive `tasks.prompt` column (applied by `ensure_column` in the
    /// baseline), so this is intentionally a no-op.
    pub(crate) fn migrate_to_v5(&self) -> Result<()> {
        Ok(())
    }

    /// v7: drop the `messages` table — the structure's first real use.
    ///
    /// It never had a production writer: only tests ever inserted into it,
    /// while three read paths served its permanently empty contents as if
    /// they were session history. Those readers now go to the conversation
    /// files, so the table is dead weight with a foreign key into
    /// `sessions`. Dropping it is exactly what the idempotent baseline
    /// cannot express, which is what the versioned dispatch is for.
    ///
    /// Safe in both directions: an older mermaid re-creates the (empty)
    /// table from its own baseline on the next open, so a rollback loses
    /// nothing that was ever there.
    pub(crate) fn migrate_to_v7(&self) -> Result<()> {
        self.conn.execute_batch(
            "DROP INDEX IF EXISTS idx_messages_session_id; DROP TABLE IF EXISTS messages;",
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    pub(crate) fn open_enables_wal_and_busy_timeout() {
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

    pub(crate) fn temp_db(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mermaid_runtime_store_{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir.join("runtime.sqlite3")
    }

    /// Hardening must not lock the owner out of the database it is protecting.
    ///
    /// The shipped version did exactly that, and nothing caught it because
    /// every existing assertion looked at the *directory* — which was always
    /// correct. The broken half was the file inside it, and no test opened one
    /// after hardening. This does, and it opens the DB the way production
    /// does rather than checking an ACE count, so it stays true whatever the
    /// implementation is.
    #[cfg(windows)]
    #[test]
    fn hardening_leaves_the_database_and_its_subdirectories_usable() {
        let path = temp_db("acl_hardening");
        let dir = path.parent().expect("temp dir").to_path_buf();

        // A DB, a subdirectory, and a file inside it: the real data dir has
        // `checkpoints/`, `memory/`, `projects/` and friends, and a fix that
        // only rescues top-level files would pass a shallower test.
        drop(RuntimeStore::open(&path).expect("seed the DB before hardening"));
        let sub = dir.join("checkpoints");
        std::fs::create_dir_all(&sub).expect("create subdir");
        std::fs::write(sub.join("existing.json"), b"{}").expect("seed a nested file");

        assert!(
            super::windows_acl::harden_data_dir(&dir),
            "icacls hardening did not run; the rest of this test would be vacuous"
        );

        // `sqlite_opens` and not `RuntimeStore::open`: the latter now repairs
        // an empty DACL, so it would paper over broken hardening and this
        // assertion would hold for the wrong reason. The claim here is that
        // hardening never needs the repair.
        assert!(
            super::windows_acl::sqlite_opens(&path),
            "hardening locked the owner out of the database it was protecting"
        );
        std::fs::read(sub.join("existing.json")).expect("a nested file must stay readable");

        // Files created AFTER hardening inherit from the directory. This is
        // the half the rejected two-pass fix broke: it left subdirectories
        // with an unflagged ACE, so anything written into `checkpoints/`
        // later came out unreadable.
        std::fs::write(sub.join("created-after.json"), b"{}").expect("write a new nested file");
        std::fs::read(sub.join("created-after.json"))
            .expect("a file created after hardening must be readable");
    }

    /// The repair path, driven from the state the bug actually produces.
    #[cfg(windows)]
    #[test]
    fn an_empty_dacl_is_repaired_on_open_rather_than_surfaced() {
        let path = temp_db("acl_repair");
        drop(RuntimeStore::open(&path).expect("seed the DB"));

        // `/inheritance:r` with no `/grant` removes every inherited ACE and
        // adds nothing, which reaches the bug's end state — an empty DACL —
        // directly. The first version of this test re-ran the shipped
        // `(OI)(CI)F` command instead and depended on that quirk producing a
        // lockout, which is one platform behavior more than the test needs.
        let stripped = std::process::Command::new("icacls")
            .arg(&path)
            .arg("/inheritance:r")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("run icacls");
        assert!(stripped.success(), "icacls must strip the DACL");

        if std::fs::read(&path).is_ok() {
            // Not every Windows can stage this. A caller holding
            // SeBackupPrivilege — every elevated GitHub Actions runner — reads
            // straight through an empty DACL, so there is no lockout here to
            // repair. Assert what is still true rather than assert something
            // false; the machines this bug actually reaches are unprivileged
            // desktops, where the branch below runs.
            println!(
                "note: this environment reads through an empty DACL; \
                 asserting the repair grant only"
            );
            assert!(
                super::windows_acl::restore_owner_access(&path),
                "the repair must still be able to grant"
            );
            RuntimeStore::open(&path).expect("open must succeed");
            return;
        }

        RuntimeStore::open(&path).expect("open must repair the ACL and succeed");
    }

    #[test]
    pub(crate) fn outcomes_round_trip_and_list_for_task() {
        let path = temp_db("outcomes");
        let store = RuntimeStore::open(&path).expect("open store");
        let task = store
            .tasks()
            .create(NewTask::new("t", "/tmp/p", "m"))
            .expect("create task");

        let first = store
            .outcomes()
            .record(NewOutcome {
                id: None,
                task_id: Some(task.id.clone()),
                tool_run_id: None,
                kind: "task_terminal".to_string(),
                label: OUTCOME_LABEL_SUCCESS.to_string(),
                reward: Some(1.0),
                source: OUTCOME_SOURCE_SYSTEM.to_string(),
                detail_json: None,
            })
            .expect("record first");
        let second = store
            .outcomes()
            .record(NewOutcome {
                id: None,
                task_id: Some(task.id.clone()),
                tool_run_id: None,
                kind: "preference".to_string(),
                label: OUTCOME_LABEL_ACCEPTED.to_string(),
                reward: None,
                source: OUTCOME_SOURCE_USER.to_string(),
                detail_json: Some("{\"chosen\":\"a\",\"rejected\":\"b\"}".to_string()),
            })
            .expect("record second");

        // get() round-trips every field, including the nullable reward and the
        // structured detail payload.
        assert_eq!(
            store.outcomes().get(&first.id).expect("get").as_ref(),
            Some(&first)
        );
        assert_eq!(first.reward, Some(1.0));
        assert_eq!(second.reward, None);
        assert_eq!(second.source, OUTCOME_SOURCE_USER);
        assert!(second.detail_json.as_deref().unwrap().contains("chosen"));

        // Both attach to the task. Assert as a set — two records created within
        // the same coarse clock tick can share a `created_at`, so the ASC order
        // between them isn't something to pin a test on.
        let for_task = store
            .outcomes()
            .list_for_task(&task.id)
            .expect("list_for_task");
        assert_eq!(for_task.len(), 2);
        let ids: std::collections::HashSet<&str> = for_task.iter().map(|o| o.id.as_str()).collect();
        assert!(ids.contains(first.id.as_str()));
        assert!(ids.contains(second.id.as_str()));

        // The global list sees them too.
        assert_eq!(store.outcomes().list(10).expect("list").len(), 2);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    pub(crate) fn claim_next_queued_orders_by_priority_then_fifo_and_skips_unclaimable() {
        let path = temp_db("claim_queue");
        let store = RuntimeStore::open(&path).expect("open store");

        // Unclaimable rows: not daemon-owned; daemon-owned but prompt-less
        // (metadata-only); daemon-owned with prompt but already running.
        store
            .tasks()
            .create(NewTask::new("cli", "/p", "m").with_prompt("x"))
            .expect("cli task");
        store
            .tasks()
            .create(NewTask::new("meta", "/p", "m").daemon_owned())
            .expect("meta task");
        let busy = store
            .tasks()
            .create(
                NewTask::new("busy", "/p", "m")
                    .daemon_owned()
                    .with_prompt("x"),
            )
            .expect("busy task");
        store
            .tasks()
            .update_status(&busy.id, TaskStatus::Running, None)
            .expect("mark busy running");

        let normal_first = store
            .tasks()
            .create(
                NewTask::new("n1", "/p", "m")
                    .daemon_owned()
                    .with_prompt("p1"),
            )
            .expect("n1");
        let low = store
            .tasks()
            .create(
                NewTask::new("l1", "/p", "m")
                    .daemon_owned()
                    .with_prompt("p2")
                    .with_priority(TaskPriority::Low),
            )
            .expect("l1");
        let high = store
            .tasks()
            .create(
                NewTask::new("h1", "/p", "m")
                    .daemon_owned()
                    .with_prompt("p-high")
                    .with_priority(TaskPriority::High),
            )
            .expect("h1");
        let normal_second = store
            .tasks()
            .create(
                NewTask::new("n2", "/p", "m")
                    .daemon_owned()
                    .with_prompt("p3"),
            )
            .expect("n2");

        // High first (despite being enqueued after the normals), then the two
        // normals FIFO, then low; each claim flips the row to Running and
        // returns the persisted prompt.
        let c1 = store.tasks().claim_next_queued().expect("claim 1").unwrap();
        assert_eq!(c1.id, high.id);
        assert_eq!(c1.status, TaskStatus::Running);
        assert_eq!(c1.prompt.as_deref(), Some("p-high"));
        let c2 = store.tasks().claim_next_queued().expect("claim 2").unwrap();
        assert_eq!(c2.id, normal_first.id);
        let c3 = store.tasks().claim_next_queued().expect("claim 3").unwrap();
        assert_eq!(c3.id, normal_second.id);
        let c4 = store.tasks().claim_next_queued().expect("claim 4").unwrap();
        assert_eq!(c4.id, low.id);
        // Queue drained: nothing claimable remains (the unclaimable trio stays).
        assert!(
            store
                .tasks()
                .claim_next_queued()
                .expect("claim 5")
                .is_none()
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    pub(crate) fn outcome_allows_null_task_and_tool_run() {
        // A free-floating outcome (no task/tool_run) is valid — task_id is
        // nullable with ON DELETE SET NULL, so the loop never loses a signal to
        // a deleted subject.
        let path = temp_db("outcomes_null");
        let store = RuntimeStore::open(&path).expect("open store");
        let rec = store
            .outcomes()
            .record(NewOutcome {
                id: None,
                task_id: None,
                tool_run_id: None,
                kind: "build".to_string(),
                label: OUTCOME_LABEL_FAILURE.to_string(),
                reward: Some(-1.0),
                source: OUTCOME_SOURCE_VERIFIER.to_string(),
                detail_json: None,
            })
            .expect("record");
        assert_eq!(rec.task_id, None);
        assert_eq!(rec.tool_run_id, None);
        assert_eq!(store.outcomes().list(10).expect("list").len(), 1);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    pub(crate) fn initializes_runtime_schema() {
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
    pub(crate) fn rejects_newer_schema_version() {
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
    pub(crate) fn checkpoint_anchor_round_trips_and_list_for_session_is_strict() {
        let path = temp_db("checkpoint_anchor");
        let store = RuntimeStore::open(&path).expect("open store");
        for (id, idx) in [("cp-a", 3_i64), ("cp-b", 5), ("cp-c", 9)] {
            store
                .checkpoints()
                .create(NewCheckpoint {
                    id: Some(id.to_string()),
                    task_id: None,
                    project_path: "/tmp/p".to_string(),
                    snapshot_path: format!("/data/checkpoints/{id}"),
                    changed_files_json: "[]".to_string(),
                    pending_action_json: None,
                    approval_id: None,
                    session_id: Some("sess-1".to_string()),
                    message_index: Some(idx),
                })
                .expect("create checkpoint");
        }
        // Unanchored + other-session rows never surface.
        store
            .checkpoints()
            .create(NewCheckpoint {
                id: Some("cp-unanchored".to_string()),
                task_id: None,
                project_path: "/tmp/p".to_string(),
                snapshot_path: "/x".to_string(),
                changed_files_json: "[]".to_string(),
                pending_action_json: None,
                approval_id: None,
                session_id: None,
                message_index: None,
            })
            .expect("create unanchored");

        let got = store.checkpoints().get("cp-a").unwrap().unwrap();
        assert_eq!(got.session_id.as_deref(), Some("sess-1"));
        assert_eq!(got.message_index, Some(3));

        // STRICT boundary: fork at k=3 keeps messages[..3]; cp-a (index 3)
        // snapshotted state from BEFORE user message 3 existed — kept prefix.
        let past = store
            .checkpoints()
            .list_for_session("sess-1", 3)
            .expect("list_for_session");
        let ids: Vec<&str> = past.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["cp-b", "cp-c"], "strict > and oldest-first");

        assert!(
            store
                .checkpoints()
                .list_for_session("sess-other", 0)
                .unwrap()
                .is_empty()
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    pub(crate) fn v7_drops_the_messages_table_and_keeps_its_session() {
        // The first genuinely non-additive migration. A v6 DB carrying the
        // table (with rows, as only tests ever produced) must open, lose the
        // table, and keep the session row that outlived it.
        let path = temp_db("v7_drop_messages");
        {
            let conn = Connection::open(&path).expect("raw open");
            conn.execute_batch(
                r"
                CREATE TABLE sessions (
                    id TEXT PRIMARY KEY,
                    project_path TEXT NOT NULL,
                    model_id TEXT NOT NULL,
                    title TEXT,
                    conversation_path TEXT,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    total_tokens INTEGER
                );
                CREATE TABLE messages (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                    role TEXT NOT NULL,
                    content_json TEXT NOT NULL,
                    created_at TEXT NOT NULL
                );
                CREATE INDEX idx_messages_session_id ON messages(session_id);
                INSERT INTO sessions (id, project_path, model_id, created_at, updated_at)
                    VALUES ('s-old', '/repo', 'm', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z');
                INSERT INTO messages (session_id, role, content_json, created_at)
                    VALUES ('s-old', 'user', '{}', '2026-01-01T00:00:00Z');
                PRAGMA user_version = 6;
                ",
            )
            .expect("seed a v6 schema");
        }

        let store = RuntimeStore::open(&path).expect("upgrade open");
        let version: i32 = store
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let table_count: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'messages'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(table_count, 0, "the messages table must be dropped");
        assert!(
            store.sessions().get("s-old").unwrap().is_some(),
            "the session it referenced must survive"
        );
        // And the baseline must not resurrect it on the next open.
        drop(store);
        let store = RuntimeStore::open(&path).expect("reopen");
        let table_count: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'messages'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(table_count, 0, "a reopen must not re-create it");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    pub(crate) fn v5_database_upgrades_with_null_checkpoint_anchors() {
        // A DB created by the previous build (schema v5, no anchor columns)
        // must open cleanly, gain the columns, and load old rows as None.
        let path = temp_db("v5_upgrade");
        {
            let conn = Connection::open(&path).expect("raw open");
            conn.execute_batch(
                r#"
                CREATE TABLE checkpoints (
                    id TEXT PRIMARY KEY,
                    task_id TEXT,
                    project_path TEXT NOT NULL,
                    snapshot_path TEXT NOT NULL,
                    changed_files_json TEXT NOT NULL,
                    pending_action_json TEXT,
                    approval_id TEXT,
                    created_at TEXT NOT NULL,
                    archived_at TEXT,
                    archive_reason TEXT
                );
                INSERT INTO checkpoints
                    (id, task_id, project_path, snapshot_path, changed_files_json, created_at)
                    VALUES ('old-cp', NULL, '/tmp/p', '/snap', '[]', '2026-01-01T00:00:00Z');
                PRAGMA user_version = 5;
                "#,
            )
            .expect("seed v5 schema");
        }
        let store = RuntimeStore::open(&path).expect("upgrade open");
        let old = store.checkpoints().get("old-cp").unwrap().unwrap();
        assert_eq!(old.session_id, None);
        assert_eq!(old.message_index, None);
        let version: i32 = store
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    pub(crate) fn init_schema_is_idempotent_across_opens() {
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

    pub(crate) fn explain_query_plan(conn: &Connection, sql: &str) -> String {
        let mut stmt = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .expect("prepare EXPLAIN QUERY PLAN");
        // Column 3 of an EQP row is the human-readable `detail` (e.g.
        // "SEARCH approvals USING INDEX idx_approvals_pending ...").
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(3))
            .expect("eqp query")
            .collect::<rusqlite::Result<Vec<String>>>()
            .expect("eqp rows");
        rows.join("\n")
    }

    #[test]
    pub(crate) fn pending_and_reconcile_scans_use_indexes() {
        // F75: the pending-approval scan and the reconcile scan must hit their
        // covering indexes rather than full-table scans.
        let path = temp_db("scan_indexes");
        let store = RuntimeStore::open(&path).expect("open");

        let index_count: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'index'
                   AND name IN ('idx_approvals_pending', 'idx_tasks_status_owner')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(index_count, 2, "F75 indexes must be created");

        // `list_pending`'s scan must use the partial pending index (it also serves
        // the ORDER BY created_at, so no separate sort).
        let plan = explain_query_plan(
            &store.conn,
            "SELECT id FROM approvals WHERE user_decision IS NULL ORDER BY created_at DESC",
        );
        assert!(
            plan.contains("idx_approvals_pending"),
            "pending scan must use idx_approvals_pending; plan was:\n{plan}"
        );

        // `reconcile_after_restart`'s scan must use the (status, owner_kind) index.
        let plan = explain_query_plan(
            &store.conn,
            "SELECT id FROM tasks WHERE status = 'running' AND owner_kind = 'daemon'",
        );
        assert!(
            plan.contains("idx_tasks_status_owner"),
            "reconcile scan must use idx_tasks_status_owner; plan was:\n{plan}"
        );

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    pub(crate) fn upgrades_from_v2_to_current_and_adds_indexes() {
        // F75/F76: a DB stamped at the previous schema version must migrate forward
        // on the next open — re-run the idempotent baseline, pick up the F75
        // indexes, and stamp the current version — exercising the per-version
        // dispatch (`from_version = 2` runs the v3 step).
        let path = temp_db("upgrade_v2");
        {
            let store = RuntimeStore::open(&path).expect("first open");
            // Simulate an older v2 DB: drop the new indexes and roll the stamp back.
            store
                .conn
                .execute_batch(
                    "DROP INDEX IF EXISTS idx_approvals_pending;
                     DROP INDEX IF EXISTS idx_tasks_status_owner;
                     PRAGMA user_version = 2;",
                )
                .expect("downgrade to v2");
        }
        let store = RuntimeStore::open(&path).expect("reopen must migrate forward");
        let version: i32 = store
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let index_count: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'index'
                   AND name IN ('idx_approvals_pending', 'idx_tasks_status_owner')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            index_count, 2,
            "forward migration must recreate the F75 indexes"
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// Strip a current DB back to the SHAPE version `version` really had —
    /// dropping the columns and tables added after it — rather than only
    /// rolling `user_version` back.
    ///
    /// The distinction is the whole point. `upgrades_from_v2_to_current_and_
    /// adds_indexes` drops two indexes and restamps, so the table it migrates
    /// still has every column a current table has. A v1 `tasks` table does not
    /// have `owner_kind`, and that is exactly what broke.
    fn downgrade_schema_to(conn: &Connection, version: i32) {
        // Indexes before the columns they cover: SQLite refuses to drop an
        // indexed column.
        if version < 6 {
            conn.execute_batch(
                "DROP INDEX IF EXISTS idx_checkpoints_session;
                 ALTER TABLE checkpoints DROP COLUMN session_id;
                 ALTER TABLE checkpoints DROP COLUMN message_index;",
            )
            .expect("undo v6");
        }
        if version < 5 {
            conn.execute_batch("ALTER TABLE tasks DROP COLUMN prompt;")
                .expect("undo v5");
        }
        if version < 4 {
            conn.execute_batch("DROP TABLE IF EXISTS outcomes;")
                .expect("undo v4");
        }
        if version < 3 {
            conn.execute_batch(
                "DROP INDEX IF EXISTS idx_approvals_pending;
                 DROP INDEX IF EXISTS idx_tasks_status_owner;",
            )
            .expect("undo v3");
        }
        if version < 2 {
            conn.execute_batch(
                "DROP INDEX IF EXISTS idx_tasks_status_owner;
                 ALTER TABLE tasks DROP COLUMN owner_kind;",
            )
            .expect("undo v2");
        }
        conn.execute_batch(&format!("PRAGMA user_version = {version};"))
            .expect("restamp");
    }

    /// Every version `init_schema` claims to accept must actually upgrade.
    ///
    /// A v1 database could not. The F75 covering index was created in the
    /// idempotent baseline, which runs before the `ensure_column` that adds
    /// the column it indexes — so the migration threw "no such column:
    /// `owner_kind`", rolled back inside its own transaction, left
    /// `user_version` unstamped, and failed the same way on every open after.
    /// Tasks, approvals, checkpoints, processes and the daemon were all
    /// unreachable, permanently, with no way forward.
    ///
    /// It survived because the two existing migration tests start at v2 and
    /// v5 — the versions that were convenient to construct. This covers the
    /// whole accepted range, including 0, which `init_schema` also routes
    /// through the migration branch.
    #[test]
    pub(crate) fn every_supported_older_version_upgrades_to_current() {
        for version in 0..SCHEMA_VERSION {
            let path = temp_db(&format!("upgrade_from_v{version}"));
            {
                let store = RuntimeStore::open(&path).expect("first open");
                downgrade_schema_to(&store.conn, version);
            }

            let store = RuntimeStore::open(&path)
                .unwrap_or_else(|e| panic!("a v{version} DB must upgrade, but: {e:#}"));

            let stamped: i32 = store
                .conn
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .expect("read user_version");
            assert_eq!(
                stamped, SCHEMA_VERSION,
                "v{version} upgraded without stamping the current version"
            );

            // The column and its index are the specific pair that broke, so
            // assert the end state rather than just "open returned Ok".
            let indexes: i64 = store
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master
                     WHERE type = 'index'
                       AND name IN ('idx_tasks_status_owner', 'idx_approvals_pending',
                                    'idx_checkpoints_session')",
                    [],
                    |r| r.get(0),
                )
                .expect("count indexes");
            assert_eq!(indexes, 3, "v{version} upgrade left indexes missing");

            // A write proves the migrated table is usable, not merely present.
            // `owner_kind` is set deliberately: it is the column the broken
            // migration never added, so a row carrying it is the end-to-end
            // claim rather than a schema inspection.
            store
                .tasks()
                .create(NewTask {
                    title: "migrated".to_string(),
                    project_path: "/p".to_string(),
                    model_id: "m".to_string(),
                    priority: TaskPriority::Normal,
                    conversation_id: None,
                    owner_kind: Some("daemon".to_string()),
                    prompt: None,
                })
                .unwrap_or_else(|e| panic!("v{version} upgraded DB must accept writes: {e:#}"));

            let _ = std::fs::remove_dir_all(path.parent().expect("temp dir"));
        }
    }

    #[test]
    pub(crate) fn task_create_commits_task_and_event_atomically() {
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
    pub(crate) fn task_lifecycle_round_trips() {
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
    pub(crate) fn approval_and_process_records_round_trip() {
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
    pub(crate) fn approval_decide_is_single_shot() {
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
    pub(crate) fn archived_approvals_and_checkpoints_are_hidden_from_visible_lists() {
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
                session_id: None,
                message_index: None,
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
    pub(crate) fn checkpoint_compaction_plugin_probe_and_pairing_round_trip() {
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
                session_id: None,
                message_index: None,
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
    pub(crate) fn pairing_token_expiry_and_revoke() {
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
    pub(crate) fn ct_eq_matches_only_identical_bytes() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
        assert!(!ct_eq(b"", b"x"));
        assert!(ct_eq(b"", b""));
    }

    #[test]
    pub(crate) fn fresh_id_is_collision_free_in_tight_loop() {
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
    pub(crate) fn tool_run_repository_redacts_arguments_and_outcomes() {
        let path = temp_db("persistence_redaction");
        let store = RuntimeStore::open(&path).expect("open store");
        let run = store
            .tool_runs()
            .start(NewToolRun {
                id: Some("toolrun-redacted".to_string()),
                task_id: None,
                turn_id: None,
                call_id: None,
                tool_name: "web_fetch".to_string(),
                args_json: Some(
                    serde_json::json!({
                        "url": "https://user:password@example.test/a?X-Goog-Credential=opaque-id&X-Goog-Signature=opaque-signature#fragment",
                        "password": "abc",
                        "token": 12345,
                        "nested": { "client_secret": true }
                    })
                    .to_string(),
                ),
            })
            .expect("start tool run");
        store
            .tool_runs()
            .finish(
                &run.id,
                "success",
                Some(
                    &serde_json::json!({
                        "model_content": "OPENAI_API_KEY=sk-abcdefghijklmnop1234\npassword=abc\nAuthorization: Bearer xyz\nAuthorization: Basic dXNlcjphYmM=\nhttps://example.test/download/sk-zyxwvutsrqponmlk9876\n-----BEGIN PRIVATE KEY-----\ncHJpdmF0ZS1tYXRlcmlhbA==\n-----END PRIVATE KEY-----"
                    })
                    .to_string(),
                ),
            )
            .expect("finish tool run");
        let persisted = store.tool_runs().get(&run.id).unwrap().unwrap();
        let args: serde_json::Value =
            serde_json::from_str(persisted.args_json.as_deref().unwrap()).unwrap();
        assert_eq!(args["password"], "[REDACTED]");
        assert_eq!(args["token"], "[REDACTED]");
        assert_eq!(args["nested"]["client_secret"], "[REDACTED]");
        let combined = format!("{:?}{:?}", persisted.args_json, persisted.output_json);
        for secret in [
            "user",
            "opaque-signature",
            "opaque-id",
            "fragment",
            "sk-abcdefghijklmnop1234",
            "password=abc",
            "Bearer xyz",
            "dXNlcjphYmM=",
            "sk-zyxwvutsrqponmlk9876",
            "cHJpdmF0ZS1tYXRlcmlhbA==",
            "-----END PRIVATE KEY-----",
            "12345",
        ] {
            assert!(
                !combined.contains(secret),
                "tool run leaked {secret}: {combined}"
            );
        }

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    pub(crate) fn ensure_column_rejects_non_identifier() {
        let path = temp_db("ensure_col");
        let store = RuntimeStore::open(&path).expect("open store");
        assert!(ensure_column(&store.conn, "approvals; DROP", "x", "TEXT").is_err());
        assert!(ensure_column(&store.conn, "approvals", "x-y", "TEXT").is_err());
        assert!(ensure_column(&store.conn, "approvals", "x", "TEXT; DROP").is_err());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    pub(crate) fn clamp_limit_never_binds_negative() {
        // #128: a huge `limit` must clamp, not wrap to a negative i64 (which
        // SQLite reads as unbounded).
        assert_eq!(clamp_limit(10), 10);
        assert_eq!(clamp_limit(usize::MAX), MAX_QUERY_LIMIT as i64);
        assert!(clamp_limit(usize::MAX) > 0);
    }

    pub(crate) fn make_approval(store: &RuntimeStore, action: &str) -> ApprovalRecord {
        store
            .approvals()
            .create(NewApproval {
                task_id: None,
                proposed_action: action.to_string(),
                risk_classification: "shell_mutation".to_string(),
                policy_decision: "ask".to_string(),
                args_summary: None,
                checkpoint_id: None,
                pending_action_json: None,
            })
            .expect("create approval")
    }

    #[test]
    pub(crate) fn approval_claim_is_single_winner_releasable_and_finalizable() {
        // #118: exactly one concurrent claim wins; a released claim re-claims; a
        // finalized one is decided and unclaimable.
        let path = temp_db("approval_claim");
        let store = RuntimeStore::open(&path).expect("open store");
        let a = make_approval(&store, "write_file a");

        assert!(store.approvals().claim(&a.id).unwrap(), "first claim wins");
        assert!(
            !store.approvals().claim(&a.id).unwrap(),
            "second claim loses"
        );

        store.approvals().release_claim(&a.id).unwrap();
        assert!(
            store.approvals().claim(&a.id).unwrap(),
            "a released claim is re-claimable (effect-failed path)"
        );

        store
            .approvals()
            .finalize_claimed(&a.id, "approved")
            .unwrap();
        assert_eq!(
            store
                .approvals()
                .get(&a.id)
                .unwrap()
                .unwrap()
                .user_decision
                .as_deref(),
            Some("approved")
        );
        assert!(
            !store.approvals().claim(&a.id).unwrap(),
            "a decided approval cannot be claimed"
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    pub(crate) fn reconcile_after_restart_recovers_running_tasks_and_claims() {
        // #120/#118: a daemon-owned Running task and an 'approving' claim left by a
        // crashed daemon are recovered on the next startup.
        let path = temp_db("reconcile");
        let store = RuntimeStore::open(&path).expect("open store");
        let task = store
            .tasks()
            .create(NewTask::new("t", "/repo", "m").daemon_owned())
            .expect("create task");
        store
            .tasks()
            .update_status(&task.id, TaskStatus::Running, None)
            .expect("mark running");
        let appr = make_approval(&store, "git push");
        assert!(store.approvals().claim(&appr.id).unwrap());

        let (tasks, claims) = store.reconcile_after_restart().expect("reconcile");
        assert_eq!((tasks, claims), (1, 1));
        assert_eq!(
            store.tasks().get(&task.id).unwrap().unwrap().status,
            TaskStatus::Failed
        );
        assert!(
            store
                .approvals()
                .get(&appr.id)
                .unwrap()
                .unwrap()
                .user_decision
                .is_none(),
            "a released claim is undecided and re-runnable"
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    pub(crate) fn reconcile_after_restart_spares_non_daemon_running_tasks() {
        // F18 (RC-E): a Running task NOT owned by the daemon (an interactive CLI
        // run sharing the store, owner_kind = NULL) must survive a daemon restart
        // — not be flipped to Failed with a spurious "interrupted" event.
        let path = temp_db("reconcile_spare_cli");
        let store = RuntimeStore::open(&path).expect("open store");

        let cli = store
            .tasks()
            .create(NewTask::new("cli run", "/repo", "m")) // no .daemon_owned()
            .expect("create cli task");
        store
            .tasks()
            .update_status(&cli.id, TaskStatus::Running, None)
            .expect("mark cli running");
        let daemon = store
            .tasks()
            .create(NewTask::new("daemon run", "/repo", "m").daemon_owned())
            .expect("create daemon task");
        store
            .tasks()
            .update_status(&daemon.id, TaskStatus::Running, None)
            .expect("mark daemon running");

        let (tasks, _claims) = store.reconcile_after_restart().expect("reconcile");
        assert_eq!(tasks, 1, "only the daemon-owned task is reset");
        assert_eq!(
            store.tasks().get(&cli.id).unwrap().unwrap().status,
            TaskStatus::Running,
            "a live CLI task must NOT be clobbered by the daemon's reconcile"
        );
        assert_eq!(
            store.tasks().get(&daemon.id).unwrap().unwrap().status,
            TaskStatus::Failed,
            "a stranded daemon task is still recovered"
        );
        // The spared CLI task gets no "interrupted" event.
        assert!(
            !store
                .tasks()
                .events(&cli.id)
                .unwrap()
                .iter()
                .any(|e| e.kind == "interrupted"),
            "the spared task must not receive a spurious interrupted event"
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    pub(crate) fn gc_prunes_old_archived_but_keeps_active() {
        // #130: GC removes archived rows past the retention window, never active
        // ones.
        let path = temp_db("gc");
        let store = RuntimeStore::open(&path).expect("open store");
        let keep = make_approval(&store, "active");
        let gone = make_approval(&store, "old archived");
        store
            .approvals()
            .archive(std::slice::from_ref(&gone.id), "test")
            .expect("archive");
        // Backdate the archive far past the window.
        store
            .conn
            .execute(
                "UPDATE approvals SET archived_at = ?2 WHERE id = ?1",
                params![gone.id, "2000-01-01T00:00:00+00:00"],
            )
            .unwrap();

        let removed = store.gc(30, 180).expect("gc");
        assert!(removed >= 1, "the old archived approval should be pruned");
        assert!(
            store.approvals().get(&gone.id).unwrap().is_none(),
            "old archived row removed"
        );
        assert!(
            store.approvals().get(&keep.id).unwrap().is_some(),
            "active row kept"
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    pub(crate) fn gc_prunes_outcomes_and_terminal_tasks_on_their_windows() {
        // R1: `gc` prunes terminal tasks past the task window and `outcomes` past
        // their own (longer) window, never touching a live task or a recent
        // outcome. When a task is pruned while its outcome survives, the outcome
        // stays with a NULL `task_id` (ON DELETE SET NULL) — the denormalized
        // `detail_json` is what keeps it usable for training after the link dies.
        let path = temp_db("gc_outcomes");
        let store = RuntimeStore::open(&path).expect("open store");
        let old = "2000-01-01T00:00:00+00:00"; // far past both windows

        // A live (queued) task must survive.
        let live = store
            .tasks()
            .create(NewTask::new("live", "/repo", "m"))
            .expect("live task");

        // An old terminal task must be pruned.
        let done = store
            .tasks()
            .create(NewTask::new("done", "/repo", "m"))
            .expect("done task");
        store
            .tasks()
            .update_status(&done.id, TaskStatus::Completed, Some("ok"))
            .expect("finish task");
        store
            .conn
            .execute(
                "UPDATE tasks SET updated_at = ?2 WHERE id = ?1",
                params![done.id, old],
            )
            .unwrap();

        // An outcome for that pruned task, still inside the (longer) outcomes
        // window: it must survive, with its link nulled and its context intact.
        let kept_outcome = store
            .outcomes()
            .record(NewOutcome {
                id: None,
                task_id: Some(done.id.clone()),
                tool_run_id: None,
                kind: "task_terminal".to_string(),
                label: OUTCOME_LABEL_SUCCESS.to_string(),
                reward: Some(1.0),
                source: OUTCOME_SOURCE_SYSTEM.to_string(),
                detail_json: Some("{\"prompt\":\"do the thing\"}".to_string()),
            })
            .expect("record kept outcome");

        // An ancient outcome, past the outcomes window: it must be pruned.
        let gone_outcome = store
            .outcomes()
            .record(NewOutcome {
                id: None,
                task_id: None,
                tool_run_id: None,
                kind: "task_terminal".to_string(),
                label: OUTCOME_LABEL_FAILURE.to_string(),
                reward: Some(-1.0),
                source: OUTCOME_SOURCE_SYSTEM.to_string(),
                detail_json: None,
            })
            .expect("record gone outcome");
        store
            .conn
            .execute(
                "UPDATE outcomes SET created_at = ?2 WHERE id = ?1",
                params![gone_outcome.id, old],
            )
            .unwrap();

        store.gc(30, 180).expect("gc");

        assert!(
            store.tasks().get(&live.id).unwrap().is_some(),
            "a live (queued) task must survive gc"
        );
        assert!(
            store.tasks().get(&done.id).unwrap().is_none(),
            "an old terminal task must be pruned"
        );
        let kept = store
            .outcomes()
            .get(&kept_outcome.id)
            .unwrap()
            .expect("the recent outcome must survive gc");
        assert!(
            kept.task_id.is_none(),
            "the pruned task's link is nulled (ON DELETE SET NULL)"
        );
        assert_eq!(
            kept.detail_json.as_deref(),
            Some("{\"prompt\":\"do the thing\"}"),
            "the denormalized training context must survive the task prune"
        );
        assert!(
            store.outcomes().get(&gone_outcome.id).unwrap().is_none(),
            "an outcome past the outcomes window must be pruned"
        );

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "a test that seeds one stale and one live row in each of four tables, runs gc \
         once, and asserts on all eight; the setup is the scenario and hiding it in fixtures \
         would separate what gc saw from what the assertions expect"
    )]
    pub(crate) fn gc_prunes_high_churn_and_old_terminal_rows_but_keeps_active() {
        // F22 (RC-F): GC prunes finished tool_runs, exited processes, old
        // compactions, and stale sessions/messages past the window — never active
        // data (a running tool_run, a live process, a fresh session).
        let path = temp_db("gc_high_churn");
        let store = RuntimeStore::open(&path).expect("open store");
        let old = "2000-01-01T00:00:00+00:00";

        // Stale session + message (deleted) vs active session + message (kept).
        let stale_session = store
            .sessions()
            .upsert(NewSession {
                id: Some("stale".to_string()),
                project_path: "/repo".to_string(),
                model_id: "m".to_string(),
                title: None,
                conversation_path: None,
                total_tokens: None,
            })
            .expect("stale session");
        let active_session = store
            .sessions()
            .upsert(NewSession {
                id: Some("active".to_string()),
                project_path: "/repo".to_string(),
                model_id: "m".to_string(),
                title: None,
                conversation_path: None,
                total_tokens: None,
            })
            .expect("active session");
        store
            .conn
            .execute(
                "UPDATE sessions SET updated_at = ?2 WHERE id = ?1",
                params![stale_session.id, old],
            )
            .unwrap();

        // Finished (old) tool_run deleted; running tool_run kept.
        store
            .tool_runs()
            .start(NewToolRun {
                id: Some("tr-finished".to_string()),
                task_id: None,
                turn_id: None,
                call_id: None,
                tool_name: "x".to_string(),
                args_json: None,
            })
            .expect("start finished tr");
        store
            .tool_runs()
            .finish("tr-finished", "success", None)
            .expect("finish tr");
        store
            .conn
            .execute(
                "UPDATE tool_runs SET finished_at = ?2 WHERE id = ?1",
                params!["tr-finished", old],
            )
            .unwrap();
        store
            .tool_runs()
            .start(NewToolRun {
                id: Some("tr-running".to_string()),
                task_id: None,
                turn_id: None,
                call_id: None,
                tool_name: "x".to_string(),
                args_json: None,
            })
            .expect("start running tr");

        // Exited (old) process deleted; running process kept.
        let exited = store
            .processes()
            .upsert(NewProcess {
                id: Some("p-exited".to_string()),
                task_id: None,
                pid: 1,
                command: "c".to_string(),
                cwd: None,
                log_path: None,
                detected_url: None,
                status: ProcessStatus::Exited,
                health: None,
            })
            .expect("exited process");
        store
            .conn
            .execute(
                "UPDATE processes SET updated_at = ?2 WHERE id = ?1",
                params![exited.id, old],
            )
            .unwrap();
        let running_proc = store
            .processes()
            .upsert(NewProcess {
                id: Some("p-running".to_string()),
                task_id: None,
                pid: 2,
                command: "c".to_string(),
                cwd: None,
                log_path: None,
                detected_url: None,
                status: ProcessStatus::Running,
                health: None,
            })
            .expect("running process");

        // Old compaction deleted.
        let comp = store
            .compactions()
            .create(NewCompaction {
                id: Some("comp-old".to_string()),
                task_id: None,
                session_id: None,
                source_token_estimate: None,
                summary_token_count: None,
                preserved_turns: None,
                archive_path: None,
                verification_status: None,
            })
            .expect("compaction");
        store
            .conn
            .execute(
                "UPDATE compactions SET created_at = ?2 WHERE id = ?1",
                params![comp.id, old],
            )
            .unwrap();

        let removed = store.gc(30, 180).expect("gc");
        // Four, not five: the message row this used to count went away with
        // the table at schema v7.
        assert!(removed >= 4, "stale rows pruned (got {removed})");
        assert!(
            store.sessions().get(&stale_session.id).unwrap().is_none(),
            "stale session gone"
        );
        assert!(
            store.sessions().get(&active_session.id).unwrap().is_some(),
            "active session kept"
        );
        assert!(
            store.tool_runs().get("tr-finished").unwrap().is_none(),
            "old finished tool_run gone"
        );
        assert!(
            store.tool_runs().get("tr-running").unwrap().is_some(),
            "running tool_run kept"
        );
        assert!(
            store.processes().get(&exited.id).unwrap().is_none(),
            "old exited process gone"
        );
        assert!(
            store.processes().get(&running_proc.id).unwrap().is_some(),
            "running process kept"
        );
        assert!(
            store.compactions().get(&comp.id).unwrap().is_none(),
            "old compaction gone"
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    pub(crate) fn task_list_skips_undecodable_status_row() {
        // F19 (RC-E): a task row whose status enum this build can't decode (a
        // different binary wrote it) is skipped, not allowed to blank the list.
        let path = temp_db("poison_task");
        let store = RuntimeStore::open(&path).expect("open store");
        let good = store
            .tasks()
            .create(NewTask::new("good", "/repo", "m"))
            .expect("create good task");
        store
            .conn
            .execute(
                "INSERT INTO tasks
                 (id, title, status, priority, project_path, model_id, created_at, updated_at)
                 VALUES ('poison', 't', 'from_the_future', 'normal', '/repo', 'm', ?1, ?1)",
                params![now_rfc3339()],
            )
            .unwrap();
        let listed = store.tasks().list(50).expect("list");
        assert_eq!(
            listed.len(),
            1,
            "the poison row is skipped, the good row remains"
        );
        assert_eq!(listed[0].id, good.id);
        // The strict get() path still surfaces the poison row as an error.
        assert!(store.tasks().get("poison").is_err(), "get() stays strict");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    pub(crate) fn checkpoint_delete_removes_row() {
        // F23 (RC-F): the on-disk dir GC drops a checkpoint's DB row so list()
        // and the on-disk dirs stay in agreement.
        let path = temp_db("ckpt_delete");
        let store = RuntimeStore::open(&path).expect("open store");
        let ckpt = store
            .checkpoints()
            .create(NewCheckpoint {
                id: Some("ckpt-1".to_string()),
                task_id: None,
                project_path: "/repo".to_string(),
                snapshot_path: "/data/checkpoints/ckpt-1".to_string(),
                changed_files_json: "[]".to_string(),
                pending_action_json: None,
                approval_id: None,
                session_id: None,
                message_index: None,
            })
            .expect("create checkpoint");
        assert!(store.checkpoints().get(&ckpt.id).unwrap().is_some());
        assert!(store.checkpoints().delete(&ckpt.id).unwrap(), "row deleted");
        assert!(
            store.checkpoints().get(&ckpt.id).unwrap().is_none(),
            "row gone"
        );
        assert!(
            !store.checkpoints().delete(&ckpt.id).unwrap(),
            "second delete is a no-op"
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn sessions_delete_cascades_rows_and_detaches_tasks() {
        let store = RuntimeStore::open(temp_db("sessions_delete")).expect("open");
        let session = store
            .sessions()
            .upsert(NewSession {
                id: Some("s-doomed".to_string()),
                project_path: "/proj".to_string(),
                model_id: "m".to_string(),
                title: Some("t".to_string()),
                conversation_path: None,
                total_tokens: None,
            })
            .unwrap();
        let task = store
            .tasks()
            .create(NewTask::new("t", "/proj", "m"))
            .unwrap();
        store
            .tasks()
            .set_conversation(&task.id, &session.id)
            .unwrap();
        assert_eq!(
            store
                .tasks()
                .get(&task.id)
                .unwrap()
                .unwrap()
                .conversation_id
                .as_deref(),
            Some("s-doomed")
        );

        assert!(store.sessions().delete("s-doomed").unwrap());
        assert!(store.sessions().get("s-doomed").unwrap().is_none());
        assert_eq!(
            store
                .tasks()
                .get(&task.id)
                .unwrap()
                .unwrap()
                .conversation_id,
            None,
            "a task pointing at the deleted session is detached"
        );
        assert!(
            !store.sessions().delete("s-doomed").unwrap(),
            "a second delete finds nothing"
        );
    }

    #[test]
    fn known_project_paths_unions_sessions_and_tasks() {
        let store = RuntimeStore::open(temp_db("known_projects")).expect("open");
        store
            .sessions()
            .upsert(NewSession {
                id: Some("s1".to_string()),
                project_path: "/a".to_string(),
                model_id: "m".to_string(),
                title: None,
                conversation_path: None,
                total_tokens: None,
            })
            .unwrap();
        store.tasks().create(NewTask::new("t", "/b", "m")).unwrap();
        store.tasks().create(NewTask::new("t2", "/a", "m")).unwrap();
        let mut paths = store.known_project_paths().unwrap();
        paths.sort();
        assert_eq!(paths, vec!["/a".to_string(), "/b".to_string()]);
        assert_eq!(
            store.sessions().ids_for_project("/a").unwrap(),
            vec!["s1".to_string()]
        );
        assert!(store.sessions().ids_for_project("/b").unwrap().is_empty());
    }
}
