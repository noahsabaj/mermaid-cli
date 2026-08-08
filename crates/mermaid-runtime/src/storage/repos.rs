//! One repository type per table. Each is a thin `&Connection` wrapper; they
//! share nothing else, which is what made this the most mechanical seam in the
//! tree — and why `TasksRepo`'s declaration had drifted 100 lines from its own
//! `impl`, with two unrelated repos in between.

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};

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
pub struct TasksRepo<'a> {
    pub(crate) conn: &'a Connection,
}

pub struct SessionsRepo<'a> {
    pub(crate) conn: &'a Connection,
}

impl SessionsRepo<'_> {
    /// # Errors
    ///
    /// Errors if the write statement fails, or if the row cannot be read back
    /// afterwards -- the reload is what produces the returned record.
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

    /// # Errors
    ///
    /// Errors if the query fails or the stored row does not decode. A row that is
    /// not there is `Ok(None)`, not an error.
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

    /// # Errors
    ///
    /// Errors if the statement fails to prepare or run, or if any row does not
    /// decode -- one undecodable row fails the whole call.
    pub fn list(&self, limit: usize) -> Result<Vec<SessionRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, project_path, model_id, title, conversation_path,
                    created_at, updated_at, total_tokens
             FROM sessions ORDER BY updated_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([clamp_limit(limit)], session_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }
}

pub struct MessagesRepo<'a> {
    pub(crate) conn: &'a Connection,
}

impl MessagesRepo<'_> {
    /// # Errors
    ///
    /// Errors if the write statement fails, or if the row cannot be read back
    /// afterwards -- the reload is what produces the returned record.
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

    /// # Errors
    ///
    /// Errors if the query fails or the stored row does not decode. A row that is
    /// not there is `Ok(None)`, not an error.
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

    /// Load a session's messages in chronological order, capped at
    /// [`MAX_SESSION_MESSAGES`] (F24/RC-F).
    ///
    /// A session transcript is otherwise unbounded, and the daemon
    /// `session_messages` path loads it whole into RAM — a pathological session
    /// could OOM the daemon. We return the **most recent** `MAX_SESSION_MESSAGES`
    /// (newest activity is what a viewer wants) but still in ascending `id`
    /// order, by taking the tail in a subquery and re-sorting it ascending.
    ///
    /// # Errors
    ///
    /// Errors if the statement fails to prepare or run, or if any row does not
    /// decode -- one undecodable row fails the whole call.
    pub fn list_for_session(&self, session_id: &str) -> Result<Vec<MessageRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, role, content_json, created_at FROM (
                 SELECT id, session_id, role, content_json, created_at
                 FROM messages WHERE session_id = ?1
                 ORDER BY id DESC LIMIT ?2
             ) ORDER BY id ASC",
        )?;
        let rows = stmt.query_map(params![session_id, MAX_SESSION_MESSAGES], message_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }
}

impl TasksRepo<'_> {
    /// # Errors
    ///
    /// Errors if the write transaction fails, or if the row cannot be read back
    /// afterwards -- the reload is what produces the returned record.
    pub fn create(&self, new: NewTask) -> Result<TaskRecord> {
        let now = now_rfc3339();
        // Owner tag isn't part of the public `TaskRecord`; move it out before the
        // record consumes the rest of `new`, then persist it on its own column.
        let owner_kind = new.owner_kind;
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
            prompt: new.prompt,
        };
        // The task row and its initial event are one logical write — commit
        // them atomically so a crash between can't leave an event-less task.
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO tasks
             (id, title, status, priority, project_path, model_id, conversation_id, created_at, updated_at, final_report, owner_kind, prompt)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
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
                owner_kind,
                record.prompt,
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

    /// # Errors
    ///
    /// Errors if the query fails or the stored row does not decode. A row that is
    /// not there is `Ok(None)`, not an error.
    pub fn get(&self, id: &str) -> Result<Option<TaskRecord>> {
        self.conn
            .query_row(
                "SELECT id, title, status, priority, project_path, model_id, conversation_id,
                        created_at, updated_at, final_report, prompt
                 FROM tasks WHERE id = ?1",
                [id],
                task_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    /// # Errors
    ///
    /// Errors if the statement fails to prepare or run, or if any row does not
    /// decode -- one undecodable row fails the whole call.
    pub fn list(&self, limit: usize) -> Result<Vec<TaskRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, status, priority, project_path, model_id, conversation_id,
                    created_at, updated_at, final_report, prompt
             FROM tasks
             ORDER BY updated_at DESC
             LIMIT ?1",
        )?;
        // F19 (RC-E): skip-and-warn a single undecodable row (e.g. a status enum
        // a different binary wrote) instead of `collect`ing a `Result` that would
        // blank the WHOLE tasks panel on one poison row.
        let rows = stmt.query_map([clamp_limit(limit)], task_from_row_opt)?;
        collect_tolerant(rows)
    }

    /// # Errors
    ///
    /// Errors if the statement fails. The work runs in a transaction, so a failure
    /// leaves the table unchanged.
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

    /// Atomically claim the next runnable queued task for the daemon scheduler:
    /// flip it to `Running` and return it, or `None` when the queue is empty.
    ///
    /// Only daemon-owned tasks WITH a persisted prompt are claimable —
    /// metadata-only tasks (interactive CLI runs, external `create_task`
    /// callers) are never executed by the scheduler. Order: priority
    /// (high → normal → low), then FIFO by `created_at` (id as tiebreaker,
    /// since two enqueues can share a coarse-clock timestamp). The claim is a
    /// single `UPDATE … RETURNING`, so concurrent claimers can never run the
    /// same task twice.
    ///
    /// # Errors
    ///
    /// Errors if the query fails or the stored row does not decode. A row that is
    /// not there is `Ok(None)`, not an error. The work runs in a transaction, so a
    /// failure leaves the table unchanged.
    pub fn claim_next_queued(&self) -> Result<Option<TaskRecord>> {
        let tx = self.conn.unchecked_transaction()?;
        let claimed = tx
            .query_row(
                "UPDATE tasks SET status = 'running', updated_at = ?1
                 WHERE id = (
                     SELECT id FROM tasks
                     WHERE status = 'queued' AND owner_kind = ?2 AND prompt IS NOT NULL
                     ORDER BY CASE priority
                                  WHEN 'high' THEN 0
                                  WHEN 'normal' THEN 1
                                  WHEN 'low' THEN 2
                                  ELSE 1
                              END,
                              created_at ASC, id ASC
                     LIMIT 1
                 )
                 RETURNING id, title, status, priority, project_path, model_id,
                           conversation_id, created_at, updated_at, final_report, prompt",
                params![now_rfc3339(), OWNER_KIND_DAEMON],
                task_from_row,
            )
            .optional()?;
        if let Some(task) = &claimed {
            tx.execute(
                "INSERT INTO task_events (task_id, kind, message, created_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    task.id,
                    "status_changed",
                    "status changed to running (claimed by scheduler)",
                    now_rfc3339(),
                ],
            )?;
        }
        tx.commit()?;
        Ok(claimed)
    }

    /// # Errors
    ///
    /// Errors if the statement fails.
    pub fn add_event(&self, task_id: &str, kind: &str, message: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO task_events (task_id, kind, message, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![task_id, kind, message, now_rfc3339()],
        )?;
        Ok(())
    }

    /// # Errors
    ///
    /// Errors if the statement fails to prepare or run, or if any row does not
    /// decode -- one undecodable row fails the whole call.
    pub fn events(&self, task_id: &str) -> Result<Vec<TaskTimelineEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, task_id, kind, message, created_at
             FROM task_events
             WHERE task_id = ?1
             ORDER BY id ASC",
        )?;
        // F19 (RC-E): one undecodable event row must not blank the whole timeline.
        let rows = stmt.query_map([task_id], task_event_from_row_opt)?;
        collect_tolerant(rows)
    }
}

pub struct ToolRunsRepo<'a> {
    pub(crate) conn: &'a Connection,
}

impl ToolRunsRepo<'_> {
    /// # Errors
    ///
    /// Errors if the write statement fails, or if the row cannot be read back
    /// afterwards -- the reload is what produces the returned record.
    pub fn start(&self, mut new: NewToolRun) -> Result<ToolRunRecord> {
        // The repository is the mandatory persistence choke point. Callers may
        // pass executable arguments unchanged; only this cloned serialized
        // representation is scrubbed before SQLite sees it.
        new.args_json = new
            .args_json
            .as_deref()
            .map(crate::redact::redact_json_text);
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

    /// # Errors
    ///
    /// Errors if the statement fails.
    pub fn finish(&self, id: &str, status: &str, output_json: Option<&str>) -> Result<()> {
        let output_json = output_json.map(crate::redact::redact_json_text);
        let changed = self.conn.execute(
            "UPDATE tool_runs
             SET status = ?2, output_json = ?3, finished_at = ?4
             WHERE id = ?1",
            params![id, status, output_json, now_rfc3339()],
        )?;
        anyhow::ensure!(changed > 0, "tool run not found: {id}");
        Ok(())
    }

    /// # Errors
    ///
    /// Errors if the query fails or the stored row does not decode. A row that is
    /// not there is `Ok(None)`, not an error.
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

    /// # Errors
    ///
    /// Errors if the statement fails to prepare or run, or if any row does not
    /// decode -- one undecodable row fails the whole call.
    pub fn list(&self, limit: usize) -> Result<Vec<ToolRunRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, task_id, turn_id, call_id, tool_name, status, args_json,
                    output_json, started_at, finished_at
             FROM tool_runs ORDER BY started_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([clamp_limit(limit)], tool_run_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }
}

pub struct OutcomesRepo<'a> {
    pub(crate) conn: &'a Connection,
}

impl OutcomesRepo<'_> {
    /// Record a verifiable outcome / reward signal for a trajectory. Append-only
    /// — the loop reads these; nothing mutates them after the fact.
    ///
    /// # Errors
    ///
    /// Errors if the write statement fails, or if the row cannot be read back
    /// afterwards -- the reload is what produces the returned record.
    pub fn record(&self, new: NewOutcome) -> Result<OutcomeRecord> {
        let id = new.id.unwrap_or_else(|| fresh_id("outcome"));
        self.conn.execute(
            "INSERT INTO outcomes
             (id, task_id, tool_run_id, kind, label, reward, source, detail_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                id,
                new.task_id,
                new.tool_run_id,
                new.kind,
                new.label,
                new.reward,
                new.source,
                new.detail_json,
                now_rfc3339(),
            ],
        )?;
        self.get(&id)?
            .context("outcome was inserted but could not be reloaded")
    }

    /// # Errors
    ///
    /// Errors if the query fails or the stored row does not decode. A row that is
    /// not there is `Ok(None)`, not an error.
    pub fn get(&self, id: &str) -> Result<Option<OutcomeRecord>> {
        self.conn
            .query_row(
                "SELECT id, task_id, tool_run_id, kind, label, reward, source,
                        detail_json, created_at
                 FROM outcomes WHERE id = ?1",
                [id],
                outcome_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    /// Every outcome recorded against one task, oldest first (the order the
    /// trajectory earned them).
    ///
    /// # Errors
    ///
    /// Errors if the statement fails to prepare or run, or if any row does not
    /// decode -- one undecodable row fails the whole call.
    pub fn list_for_task(&self, task_id: &str) -> Result<Vec<OutcomeRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, task_id, tool_run_id, kind, label, reward, source,
                    detail_json, created_at
             FROM outcomes WHERE task_id = ?1 ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map([task_id], outcome_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// # Errors
    ///
    /// Errors if the statement fails to prepare or run, or if any row does not
    /// decode -- one undecodable row fails the whole call.
    pub fn list(&self, limit: usize) -> Result<Vec<OutcomeRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, task_id, tool_run_id, kind, label, reward, source,
                    detail_json, created_at
             FROM outcomes ORDER BY created_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([clamp_limit(limit)], outcome_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }
}

pub(crate) fn outcome_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<OutcomeRecord> {
    Ok(OutcomeRecord {
        id: row.get(0)?,
        task_id: row.get(1)?,
        tool_run_id: row.get(2)?,
        kind: row.get(3)?,
        label: row.get(4)?,
        reward: row.get(5)?,
        source: row.get(6)?,
        detail_json: row.get(7)?,
        created_at: row.get(8)?,
    })
}

pub struct ApprovalsRepo<'a> {
    pub(crate) conn: &'a Connection,
}

impl ApprovalsRepo<'_> {
    /// # Errors
    ///
    /// Errors if the write statement fails, or if the row cannot be read back
    /// afterwards -- the reload is what produces the returned record.
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

    /// # Errors
    ///
    /// Errors if the query fails or the stored row does not decode. A row that is
    /// not there is `Ok(None)`, not an error.
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

    /// # Errors
    ///
    /// Errors if the statement fails.
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
            "approval {id} cannot be decided (already decided, archived, or not found)"
        );
        Ok(())
    }

    /// Atomically claim an undecided approval for replay (#118). Sets
    /// `user_decision='approving'` only when it is currently NULL and
    /// un-archived, and reports whether THIS caller won the claim. Two concurrent
    /// `approve <id>` calls race this single UPDATE; exactly one sees
    /// `rows_affected == 1` and runs the un-rollback-able effect, the other sees
    /// `false` and bails — so the effect can't fire twice. A claim that crashes
    /// before finalizing is reset to NULL by the daemon's startup reconcile.
    ///
    /// # Errors
    ///
    /// Errors if the UPDATE fails. Losing the race is `Ok(false)`, not an error:
    /// the row was already decided, already claimed, or archived.
    pub fn claim(&self, id: &str) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE approvals
             SET user_decision = 'approving'
             WHERE id = ?1 AND user_decision IS NULL AND archived_at IS NULL",
            params![id],
        )?;
        Ok(changed == 1)
    }

    /// Release a claim taken by [`Self::claim`] back to undecided, so the action
    /// stays re-runnable after the replay effect failed.
    ///
    /// # Errors
    ///
    /// Errors if the statement fails.
    pub fn release_claim(&self, id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE approvals SET user_decision = NULL
             WHERE id = ?1 AND user_decision = 'approving'",
            params![id],
        )?;
        Ok(())
    }

    /// Finalize a claimed approval's decision (the `approving` → terminal-value
    /// transition that [`Self::decide`]'s `WHERE user_decision IS NULL` can't make).
    ///
    /// # Errors
    ///
    /// Errors if the statement fails.
    pub fn finalize_claimed(&self, id: &str, user_decision: &str) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE approvals
             SET user_decision = ?2, decided_at = ?3
             WHERE id = ?1 AND user_decision = 'approving'",
            params![id, user_decision, now_rfc3339()],
        )?;
        anyhow::ensure!(changed > 0, "approval {id} was not in the claimed state");
        Ok(())
    }

    /// # Errors
    ///
    /// Errors if the underlying query fails or any row does not decode.
    pub fn list_pending(&self) -> Result<Vec<ApprovalRecord>> {
        self.list_pending_with_archived(false)
    }

    /// # Errors
    ///
    /// Errors if the underlying query fails or any row does not decode.
    pub fn list_pending_all(&self) -> Result<Vec<ApprovalRecord>> {
        self.list_pending_with_archived(true)
    }

    /// # Errors
    ///
    /// Errors if the statement fails to prepare or run, or if any row does not
    /// decode -- one undecodable row fails the whole call.
    pub fn list_all(&self, limit: usize) -> Result<Vec<ApprovalRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, task_id, proposed_action, risk_classification, policy_decision,
                    user_decision, args_summary, checkpoint_id, pending_action_json,
                    created_at, decided_at, archived_at, archive_reason
             FROM approvals
             ORDER BY created_at DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map([clamp_limit(limit)], approval_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub(crate) fn list_pending_with_archived(
        &self,
        include_archived: bool,
    ) -> Result<Vec<ApprovalRecord>> {
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

    /// # Errors
    ///
    /// Errors if any one of the per-id updates fails, and stops there -- the ids
    /// before it stay archived, because the loop runs outside a transaction. The
    /// count is of rows actually changed, so ids already archived add nothing and
    /// are not an error.
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

    /// # Errors
    ///
    /// Errors if the count query fails.
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
    pub(crate) conn: &'a Connection,
}

impl ProcessesRepo<'_> {
    /// # Errors
    ///
    /// Errors if the write statement fails, or if the row cannot be read back
    /// afterwards -- the reload is what produces the returned record.
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

    /// # Errors
    ///
    /// Errors if the query fails or the stored row does not decode. A row that is
    /// not there is `Ok(None)`, not an error.
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

    /// # Errors
    ///
    /// Errors if the statement fails to prepare or run, or if any row does not
    /// decode -- one undecodable row fails the whole call.
    pub fn list(&self, limit: usize) -> Result<Vec<ProcessRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, task_id, pid, command, cwd, log_path, detected_url, status, health,
                    created_at, updated_at
             FROM processes
             ORDER BY updated_at DESC
             LIMIT ?1",
        )?;
        // F19 (RC-E): skip-and-warn an undecodable row (e.g. a status enum a
        // different binary wrote) rather than blanking the whole processes panel.
        let rows = stmt.query_map([clamp_limit(limit)], process_from_row_opt)?;
        collect_tolerant(rows)
    }
}

pub struct CheckpointsRepo<'a> {
    pub(crate) conn: &'a Connection,
}

impl CheckpointsRepo<'_> {
    /// # Errors
    ///
    /// Errors if the write statement fails, or if the row cannot be read back
    /// afterwards -- the reload is what produces the returned record.
    pub fn create(&self, new: NewCheckpoint) -> Result<CheckpointRecord> {
        let id = new.id.unwrap_or_else(|| fresh_id("checkpoint"));
        self.conn.execute(
            "INSERT INTO checkpoints
             (id, task_id, project_path, snapshot_path, changed_files_json,
              pending_action_json, approval_id, created_at, session_id, message_index)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                id,
                new.task_id,
                new.project_path,
                new.snapshot_path,
                new.changed_files_json,
                new.pending_action_json,
                new.approval_id,
                now_rfc3339(),
                new.session_id,
                new.message_index,
            ],
        )?;
        self.get(&id)?
            .context("checkpoint was inserted but could not be reloaded")
    }

    /// # Errors
    ///
    /// Errors if the query fails or the stored row does not decode. A row that is
    /// not there is `Ok(None)`, not an error.
    pub fn get(&self, id: &str) -> Result<Option<CheckpointRecord>> {
        self.conn
            .query_row(
                "SELECT id, task_id, project_path, snapshot_path, changed_files_json,
                        pending_action_json, approval_id, created_at, archived_at, archive_reason,
                        session_id, message_index
                 FROM checkpoints WHERE id = ?1",
                [id],
                checkpoint_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    /// # Errors
    ///
    /// Errors if the statement fails.
    pub fn set_approval(&self, id: &str, approval_id: &str) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE checkpoints SET approval_id = ?2 WHERE id = ?1",
            params![id, approval_id],
        )?;
        anyhow::ensure!(changed > 0, "checkpoint not found: {id}");
        Ok(())
    }

    /// Delete a checkpoint row outright. Returns whether a row was removed.
    ///
    /// F23 (RC-F): coordinates the on-disk checkpoint-dir GC
    /// ([`crate::checkpoint::gc_old_checkpoint_dirs`]) with the DB. The dir GC
    /// prunes by mtime regardless of archive state, while storage [`Self`] /
    /// `gc()` only removes ARCHIVED checkpoint rows — so a never-archived old
    /// checkpoint would lose its directory while its row survived, and a later
    /// `restore_checkpoint` would fail on the missing manifest. The dir GC now
    /// calls this so `list()` and the on-disk dirs stay in agreement.
    ///
    /// # Errors
    ///
    /// Errors if the DELETE fails. A checkpoint that was not there is `Ok(false)`,
    /// not an error.
    pub fn delete(&self, id: &str) -> Result<bool> {
        let changed = self
            .conn
            .execute("DELETE FROM checkpoints WHERE id = ?1", params![id])?;
        Ok(changed > 0)
    }

    /// # Errors
    ///
    /// Errors if the statement fails.
    pub fn list(&self, limit: usize) -> Result<Vec<CheckpointRecord>> {
        self.list_with_archived(limit, false)
    }

    /// # Errors
    ///
    /// Errors if the statement fails.
    pub fn list_all(&self, limit: usize) -> Result<Vec<CheckpointRecord>> {
        self.list_with_archived(limit, true)
    }

    pub(crate) fn list_with_archived(
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
                    pending_action_json, approval_id, created_at, archived_at, archive_reason,
                    session_id, message_index
             FROM checkpoints {archived_filter} ORDER BY created_at DESC LIMIT ?1"
        ))?;
        let rows = stmt.query_map([clamp_limit(limit)], checkpoint_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Unarchived checkpoints of `session_id` anchored STRICTLY past
    /// `after_message_index`, oldest first. Strict `>` is the fork-boundary
    /// invariant: a fork at user-message index `k` keeps `messages[..k]`, and
    /// a checkpoint stamped `message_index == k` snapshotted state from
    /// BEFORE that user message existed — it belongs to the kept prefix, not
    /// the discarded timeline. Oldest-first because each checkpoint is a
    /// PRE-mutation snapshot: the oldest one past the cut holds the file
    /// state closest to the fork point.
    ///
    /// # Errors
    ///
    /// Errors if the statement fails to prepare or run, or if any row does not
    /// decode -- one undecodable row fails the whole call.
    pub fn list_for_session(
        &self,
        session_id: &str,
        after_message_index: i64,
    ) -> Result<Vec<CheckpointRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, task_id, project_path, snapshot_path, changed_files_json,
                    pending_action_json, approval_id, created_at, archived_at, archive_reason,
                    session_id, message_index
             FROM checkpoints
             WHERE session_id = ?1 AND message_index > ?2 AND archived_at IS NULL
             ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map(
            params![session_id, after_message_index],
            checkpoint_from_row,
        )?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// # Errors
    ///
    /// Errors if any one of the per-id updates fails, and stops there -- the ids
    /// before it stay archived, because the loop runs outside a transaction. The
    /// count is of rows actually changed, so ids already archived add nothing and
    /// are not an error.
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

    /// # Errors
    ///
    /// Errors if the count query fails.
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
    pub(crate) conn: &'a Connection,
}

impl CompactionsRepo<'_> {
    /// # Errors
    ///
    /// Errors if the write statement fails, or if the row cannot be read back
    /// afterwards -- the reload is what produces the returned record.
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

    /// # Errors
    ///
    /// Errors if the query fails or the stored row does not decode. A row that is
    /// not there is `Ok(None)`, not an error.
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

    /// # Errors
    ///
    /// Errors if the statement fails to prepare or run, or if any row does not
    /// decode -- one undecodable row fails the whole call.
    pub fn list(&self, limit: usize) -> Result<Vec<CompactionRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, task_id, session_id, source_token_estimate, summary_token_count,
                    preserved_turns, archive_path, verification_status, created_at
             FROM compactions ORDER BY created_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([clamp_limit(limit)], compaction_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }
}

pub struct PluginsRepo<'a> {
    pub(crate) conn: &'a Connection,
}

impl PluginsRepo<'_> {
    /// # Errors
    ///
    /// Errors if the write statement fails, or if the row cannot be read back
    /// afterwards -- the reload is what produces the returned record.
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

    /// # Errors
    ///
    /// Errors if the query fails or the stored row does not decode. A row that is
    /// not there is `Ok(None)`, not an error.
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

    /// # Errors
    ///
    /// Errors if the statement fails to prepare or run, or if any row does not
    /// decode -- one undecodable row fails the whole call.
    pub fn list(&self) -> Result<Vec<PluginInstallRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, source, version, enabled, manifest_json, installed_at, updated_at
             FROM plugin_installs ORDER BY name ASC",
        )?;
        let rows = stmt.query_map([], plugin_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// # Errors
    ///
    /// Errors if the statement fails.
    pub fn set_enabled(&self, id: &str, enabled: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE plugin_installs SET enabled = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, if enabled { 1 } else { 0 }, now_rfc3339()],
        )?;
        Ok(())
    }
}

pub struct ProviderProbesRepo<'a> {
    pub(crate) conn: &'a Connection,
}

impl ProviderProbesRepo<'_> {
    /// # Errors
    ///
    /// Errors if the write statement fails, or if the row cannot be read back
    /// afterwards -- the reload is what produces the returned record.
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

    /// # Errors
    ///
    /// Errors if the query fails or the stored row does not decode. A row that is
    /// not there is `Ok(None)`, not an error.
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

    /// # Errors
    ///
    /// Errors if the statement fails to prepare or run, or if any row does not
    /// decode -- one undecodable row fails the whole call.
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
    pub(crate) conn: &'a Connection,
}

impl PairingTokensRepo<'_> {
    /// # Errors
    ///
    /// Errors if the write statement fails, or if the row cannot be read back
    /// afterwards -- the reload is what produces the returned record.
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

    /// # Errors
    ///
    /// Errors if the query fails or the stored row does not decode. A row that is
    /// not there is `Ok(None)`, not an error.
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
    ///
    /// # Errors
    ///
    /// Errors if the query fails or a row does not decode. No match is `Ok(None)`,
    /// not an error, and so is a token that matches but has expired.
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

    /// # Errors
    ///
    /// Errors if the statement fails to prepare or run, or if any row does not
    /// decode -- one undecodable row fails the whole call.
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
    ///
    /// # Errors
    ///
    /// Errors if the underlying query fails or any row does not decode.
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

    /// # Errors
    ///
    /// Errors if the statement fails.
    pub fn mark_used(&self, id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE pairing_tokens SET last_used_at = ?2 WHERE id = ?1 AND enabled = 1",
            params![id, now_rfc3339()],
        )?;
        Ok(())
    }

    /// Revoke a token by disabling it. Returns `true` if a live token was
    /// revoked, `false` if it was already disabled or unknown.
    ///
    /// # Errors
    ///
    /// Errors if the UPDATE fails. A token that was already revoked, or absent, is
    /// `Ok(false)`, not an error.
    pub fn revoke(&self, id: &str) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE pairing_tokens SET enabled = 0 WHERE id = ?1 AND enabled = 1",
            params![id],
        )?;
        Ok(changed > 0)
    }
}
