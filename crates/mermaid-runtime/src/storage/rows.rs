//! Row decoding, and the tolerant layer that lets one unreadable row not sink
//! a whole listing.

use std::fmt;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::types::Type;

use super::*;

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
/// Add `column` to `table` if it is missing. Returns `true` iff the column was
/// just created (so the caller can run a one-time backfill).
///
/// SQL identifiers cannot be bound as `?` parameters, so `table`/`column`/
/// `definition` are interpolated. All call sites pass compile-time constants
/// today; the validation below makes that a hard invariant rather than a latent
/// injection footgun if a future caller ever threads in dynamic input.
pub(crate) fn ensure_column(
    conn: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> Result<bool> {
    pub(crate) fn is_sql_identifier(s: &str) -> bool {
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

/// Environment override for the data directory, checked ahead of the platform
/// location. The data-dir twin of `app::config::CONFIG_DIR_ENV`, and it exists
/// for the same reason: `ProjectDirs` resolves a Windows known folder that no
/// environment variable redirects, so tests spawning the real binary wrote
/// checkpoints and process rows into the developer's own store.
pub const DATA_DIR_ENV: &str = "MERMAID_DATA_DIR";

/// The app data dir: [`DATA_DIR_ENV`] when set, else the platform location, or
/// `~/.local/share/mermaid` when the platform has none.
///
/// # Errors
///
/// Only the fallback path failing, when neither `HOME` nor `USERPROFILE` is
/// set. The directory is not created or checked for here.
pub fn data_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os(DATA_DIR_ENV).filter(|dir| !dir.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    if let Some(proj_dirs) = ProjectDirs::from("", "", "mermaid") {
        return Ok(proj_dirs.data_dir().to_path_buf());
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .context("could not determine home directory")?;
    Ok(PathBuf::from(home).join(".local/share/mermaid"))
}

pub(crate) fn session_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRecord> {
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

pub(crate) fn message_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<MessageRecord> {
    Ok(MessageRecord {
        id: row.get("id")?,
        session_id: row.get("session_id")?,
        role: row.get("role")?,
        content_json: row.get("content_json")?,
        created_at: row.get("created_at")?,
    })
}

pub(crate) fn task_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskRecord> {
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
        prompt: row.get("prompt")?,
    })
}

pub(crate) fn process_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ProcessRecord> {
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

pub(crate) fn tool_run_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ToolRunRecord> {
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

pub(crate) fn approval_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ApprovalRecord> {
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

pub(crate) fn checkpoint_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CheckpointRecord> {
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
        session_id: row.get("session_id")?,
        message_index: row.get("message_index")?,
    })
}

pub(crate) fn compaction_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CompactionRecord> {
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

pub(crate) fn plugin_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PluginInstallRecord> {
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

pub(crate) fn provider_probe_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<ProviderProbeRecord> {
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

pub(crate) fn pairing_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PairingTokenRecord> {
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

pub(crate) fn task_event_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskTimelineEvent> {
    Ok(TaskTimelineEvent {
        id: row.get("id")?,
        task_id: row.get("task_id")?,
        kind: row.get("kind")?,
        message: row.get("message")?,
        created_at: row.get("created_at")?,
    })
}

/// Whether a row error is a per-row DECODE failure — a value a different binary
/// wrote that this build can't parse: an unknown enum
/// ([`rusqlite::Error::FromSqlConversionFailure`], how `task_from_row` /
/// `process_from_row` surface an unknown status) or a column type mismatch
/// ([`rusqlite::Error::InvalidColumnType`]). F19 (RC-E): the list/events paths
/// skip-and-warn on these so one poison row can't blank an entire panel, while a
/// genuine infrastructure error (a locked DB, a dropped column) still propagates.
pub(crate) fn is_row_decode_error(err: &rusqlite::Error) -> bool {
    matches!(
        err,
        rusqlite::Error::FromSqlConversionFailure(..) | rusqlite::Error::InvalidColumnType(..)
    )
}

/// Tolerant [`task_from_row`]: `Ok(None)` (with a warning) for a row this build
/// can't decode, so [`TasksRepo::list`] skips it instead of failing the list.
pub(crate) fn task_from_row_opt(row: &rusqlite::Row<'_>) -> rusqlite::Result<Option<TaskRecord>> {
    match task_from_row(row) {
        Ok(record) => Ok(Some(record)),
        Err(err) if is_row_decode_error(&err) => {
            tracing::warn!(error = %err, "skipping task row this build can't decode (version skew?)");
            Ok(None)
        },
        Err(err) => Err(err),
    }
}

/// Tolerant [`process_from_row`] — see [`task_from_row_opt`].
pub(crate) fn process_from_row_opt(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<Option<ProcessRecord>> {
    match process_from_row(row) {
        Ok(record) => Ok(Some(record)),
        Err(err) if is_row_decode_error(&err) => {
            tracing::warn!(error = %err, "skipping process row this build can't decode (version skew?)");
            Ok(None)
        },
        Err(err) => Err(err),
    }
}

/// Tolerant [`task_event_from_row`] — see [`task_from_row_opt`].
pub(crate) fn task_event_from_row_opt(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<Option<TaskTimelineEvent>> {
    match task_event_from_row(row) {
        Ok(record) => Ok(Some(record)),
        Err(err) if is_row_decode_error(&err) => {
            tracing::warn!(error = %err, "skipping task event row this build can't decode");
            Ok(None)
        },
        Err(err) => Err(err),
    }
}

/// Collect rows from a tolerant decoder (one that yields `Ok(None)` for a
/// skipped poison row), dropping the `None`s and propagating any real error.
pub(crate) fn collect_tolerant<T>(
    rows: impl Iterator<Item = rusqlite::Result<Option<T>>>,
) -> Result<Vec<T>> {
    let mut out = Vec::new();
    for row in rows {
        if let Some(item) = row? {
            out.push(item);
        }
    }
    Ok(out)
}

pub(crate) fn enum_from_sql_error(
    column: &'static str,
    value: String,
    source: UnknownRuntimeEnum,
) -> rusqlite::Error {
    let _ = value;
    rusqlite::Error::FromSqlConversionFailure(column_index(column), Type::Text, Box::new(source))
}

pub(crate) fn column_index(column: &str) -> usize {
    match column {
        "status" => 2,
        "priority" => 3,
        _ => 0,
    }
}

#[derive(Debug)]
pub(crate) struct UnknownRuntimeEnum {
    kind: &'static str,
    value: String,
}

impl UnknownRuntimeEnum {
    pub(crate) fn new(kind: &'static str, value: &str) -> Self {
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

pub(crate) fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Constant-time byte-slice equality. Unlike `==` (or a SQL `=`), it never
/// short-circuits on the first differing byte, so it leaks no timing signal
/// about how much of a secret matched. Lengths are compared first; the length
/// of a token hash is fixed and not secret.
pub(crate) fn ct_eq(a: &[u8], b: &[u8]) -> bool {
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
pub(crate) fn is_expired(expires_at: Option<&str>, now: chrono::DateTime<chrono::Utc>) -> bool {
    match expires_at {
        None => false,
        Some(raw) => match chrono::DateTime::parse_from_rfc3339(raw) {
            Ok(dt) => dt <= now,
            Err(_) => true,
        },
    }
}

/// Upper bound on any `LIMIT` we bind. A caller-supplied `limit` (e.g. a daemon
/// request body's `limit`) can be a huge `u64` that, cast straight to `i64`,
/// wraps negative — and SQLite reads a negative `LIMIT` as *unbounded*, so the
/// query returns every row (#128). Clamp at the `usize` level before the cast.
pub(crate) const MAX_QUERY_LIMIT: usize = 10_000;

pub(crate) fn clamp_limit(limit: usize) -> i64 {
    limit.min(MAX_QUERY_LIMIT) as i64
}

/// Upper bound on the rows [`MessagesRepo::list_for_session`] returns (F24/RC-F).
/// A session transcript is unbounded and the daemon `session_messages` path loads
/// it whole into RAM; this caps the worst-case load at the most recent N messages
/// so one pathological session can't OOM the daemon. 5000 turns is far beyond any
/// real interactive session yet bounds memory.
pub(crate) const MAX_SESSION_MESSAGES: i64 = 5_000;

pub(crate) fn fresh_id(prefix: &str) -> String {
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

/// Acquire an exclusive, auto-released advisory lock on `path` — a process
/// singleton guard for the daemon (#131). Returns the held `File` on success
/// (keep it alive to hold the lock), or `None` if another process already holds
/// it. `flock` releases automatically when the file is dropped OR the process
/// exits/crashes, so a dead holder never wedges the lock the way an `O_EXCL`
/// pidfile would. Holding it across the socket probe → unlink → bind closes that
/// TOCTOU: two daemons can't both decide a stale socket is theirs to rebind.
///
/// Unix-only: it backs the `#[cfg(unix)]` daemon singleton and relies on
/// `flock`, which `rustix` exposes only on Unix targets.
///
/// # Errors
///
/// Opening `path` (a missing parent directory, no permission), and any `flock`
/// failure other than the lock being taken. That one case is `Ok(None)`, not
/// an error — losing the singleton race is the answer this exists to give.
#[cfg(unix)]
pub fn try_exclusive_lock(path: &std::path::Path) -> std::io::Result<Option<std::fs::File>> {
    use rustix::fs::{FlockOperation, flock};
    // A lockfile's content is irrelevant — only the flock matters — so don't
    // truncate (avoids a needless write and any truncate/lock ordering race).
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)?;
    match flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(Some(file)),
        Err(rustix::io::Errno::WOULDBLOCK) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The matched pair for the override: set, it redirects `data_dir()`
    /// wholesale (the isolation the integration suites rely on so a test run
    /// can never write the developer's real runtime store — Windows resolves
    /// the platform location through a known folder no HOME/XDG var moves);
    /// empty, it is "unset", so a stray `MERMAID_DATA_DIR=` in a shell profile
    /// cannot silently point the store at the current directory.
    #[test]
    fn data_dir_env_override_wins_and_empty_is_unset() {
        let sandbox = std::env::temp_dir().join("mermaid-data-dir-override-test");
        temp_env::with_var(
            DATA_DIR_ENV,
            Some(sandbox.to_str().expect("utf8 temp path")),
            || {
                assert_eq!(data_dir().expect("override resolves"), sandbox);
            },
        );
        temp_env::with_var(DATA_DIR_ENV, Some(""), || {
            let resolved = data_dir().expect("platform dir resolves");
            assert_ne!(resolved, PathBuf::from(""), "empty must not become cwd");
            assert!(resolved.is_absolute(), "got {}", resolved.display());
        });
    }
}
