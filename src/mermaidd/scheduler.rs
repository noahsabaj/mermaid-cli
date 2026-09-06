//! The durable task queue: one permit per concurrent task, claims from the
//! store, execution through the headless runner, terminal status and outcome
//! recording, and the task-to-session backlink.

use anyhow::Result;

/// The daemon task scheduler. `run` requests only *enqueue*; the drain loop
/// executes queued tasks bounded by `daemon.max_concurrent_tasks` permits, so
/// a burst of runs proceeds serially (or up to the configured width) instead
/// of stampeding the GPU with N simultaneous agent loops. `running` maps each
/// in-flight task to its cancellation token — the handle `cancel_task` uses to
/// stop it.
pub(super) struct Scheduler {
    pub(super) permits: std::sync::Arc<tokio::sync::Semaphore>,
    pub(super) running:
        std::sync::Mutex<std::collections::HashMap<String, tokio_util::sync::CancellationToken>>,
    pub(super) wake: tokio::sync::Notify,
    pub(super) task_timeout: Option<std::time::Duration>,
    /// Live `RunEvent` broadcast per task, for `subscribe_task`. GET-OR-CREATE
    /// from BOTH the executor (which wires it into `RunOptions.event_tx`) and
    /// the subscribe handler — so subscribing to a still-QUEUED task works:
    /// the subscriber holds a receiver on the sender the executor later uses.
    /// Entries are removed by a drop guard after the terminal status persists.
    pub(super) streams: std::sync::Mutex<
        std::collections::HashMap<String, tokio::sync::broadcast::Sender<mermaid_domain::RunEvent>>,
    >,
    /// Live mailbox per running task, for `send_to_task`. Registered when the
    /// run publishes its `EngineHandle` and dropped with its stream. Absent
    /// means "not running right now", which is exactly what the handler
    /// reports -- unlike `streams`, there is nothing useful to create on
    /// demand, because a queued task has no reducer to talk to yet.
    pub(super) mailboxes: std::sync::Mutex<
        std::collections::HashMap<String, crate::engine::EngineHandle<mermaid_domain::RunEvent>>,
    >,
}

impl Scheduler {
    /// The task's live event sender, created on first request. Capacity 1024:
    /// a lagged subscriber gets a `RecvError::Lagged` marker, not backpressure
    /// into the run.
    pub(super) fn stream_for(
        &self,
        task_id: &str,
    ) -> tokio::sync::broadcast::Sender<mermaid_domain::RunEvent> {
        self.streams
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(task_id.to_string())
            .or_insert_with(|| tokio::sync::broadcast::channel(1024).0)
            .clone()
    }

    pub(super) fn register_mailbox(
        &self,
        task_id: &str,
        handle: crate::engine::EngineHandle<mermaid_domain::RunEvent>,
    ) {
        self.mailboxes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(task_id.to_string(), handle);
    }

    pub(super) fn mailbox_for(
        &self,
        task_id: &str,
    ) -> Option<crate::engine::EngineHandle<mermaid_domain::RunEvent>> {
        self.mailboxes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(task_id)
            .cloned()
    }

    pub(super) fn drop_mailbox(&self, task_id: &str) {
        self.mailboxes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(task_id);
    }

    pub(super) fn drop_stream(&self, task_id: &str) {
        self.streams
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(task_id);
    }
}

pub(super) static SCHEDULER: std::sync::OnceLock<Scheduler> = std::sync::OnceLock::new();

pub(super) fn scheduler() -> &'static Scheduler {
    SCHEDULER
        .get()
        .expect("scheduler is initialized in main before serving")
}

/// Drain queued daemon tasks forever: take a permit, claim the next queued
/// task, execute it, repeat. Queued tasks left over from a previous daemon are
/// picked up automatically on restart — the queue is durable.
pub(super) async fn scheduler_drain_loop() {
    let sched = scheduler();
    loop {
        let permit = match sched.permits.clone().acquire_owned().await {
            Ok(permit) => permit,
            // A closed semaphore means the process is going down.
            Err(_) => return,
        };
        // With a permit in hand, wait until a task is claimable. Holding the
        // permit while idle is fine — only executions consume permits.
        let task = loop {
            let claimed =
                mermaid_runtime::with_shared_store(|store| store.tasks().claim_next_queued());
            match claimed {
                Ok(Some(task)) => break task,
                Ok(None) => {
                    // `notify_one` stores a wakeup when no waiter is parked, so
                    // the enqueue→notify path can't be lost; the periodic tick
                    // is belt-and-braces.
                    tokio::select! {
                        _ = sched.wake.notified() => {},
                        _ = tokio::time::sleep(std::time::Duration::from_secs(3)) => {},
                    }
                },
                Err(error) => {
                    tracing::warn!(error = %error, "scheduler claim failed; retrying");
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                },
            }
        };
        tokio::spawn(execute_claimed_task(task, permit));
    }
}

/// RAII guard that removes a task's token from the scheduler's running map when
/// it drops — on the normal return and, crucially, if the run panics. Without
/// it a panicking run would leak the map entry (the permit is freed by its own
/// drop and the row is reconciled on restart, but the map would keep growing).
pub(super) struct RunningGuard {
    pub(super) sched: &'static Scheduler,
    pub(super) task_id: String,
}

impl Drop for RunningGuard {
    fn drop(&mut self) {
        // A destructor must not panic (a double-panic aborts), so recover a
        // poisoned lock instead of `expect`.
        self.sched
            .running
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.task_id);
    }
}

/// Execute one claimed task to its terminal status, holding its concurrency
/// permit for the duration and registering a cancellation token so
/// `cancel_task` can reach it.
pub(super) async fn execute_claimed_task(
    task: mermaid_runtime::TaskRecord,
    permit: tokio::sync::OwnedSemaphorePermit,
) {
    let _permit = permit;
    let sched = scheduler();
    let Some(prompt) = task.prompt.clone() else {
        // Unreachable via `claim_next_queued` (it filters `prompt IS NOT
        // NULL`), but never leave a row wedged in Running if it happens.
        persist_terminal_status(
            &task.id,
            mermaid_runtime::TaskStatus::Failed,
            "task has no persisted prompt",
        )
        .await;
        return;
    };
    let token = tokio_util::sync::CancellationToken::new();
    sched
        .running
        .lock()
        .expect("scheduler running map poisoned")
        .insert(task.id.clone(), token.clone());
    // Removed on every exit path (including a panic in the run below) by the
    // guard's Drop, so the running map can't leak a dead task's entry.
    let _running_guard = RunningGuard {
        sched,
        task_id: task.id.clone(),
    };

    let _ = mermaid_runtime::run_plugin_hooks(
        "task_start",
        &serde_json::json!({
            "id": task.id.clone(),
            "title": task.title.clone(),
            "project_path": task.project_path.clone(),
            "model_id": task.model_id.clone(),
        }),
    );

    let config = crate::app::load_config().unwrap_or_default();
    // Live event stream for `subscribe_task` attachments (pre-run
    // subscribers already hold receivers on this same sender).
    let event_tx = sched.stream_for(&task.id);
    // Stamp the backlink when the run ANNOUNCES its session, not only when
    // it ends. `tasks.conversation_id` is the one key from a task to its
    // session log, and a mid-run `subscribe_task` attach needs it while the
    // run is still going — it is what turns a from-now attach into one that
    // can replay what it missed. The end-of-run stamp below stays as the
    // authority; this is the same value, earlier.
    let _backlink = tokio::spawn(early_backlink(event_tx.subscribe(), task.id.clone()));
    // The run's mailbox, so a `send_to_task` can reach it while it works. The
    // handle arrives as soon as the engine exists -- before the first model
    // call -- and registering it from its own task keeps the executor from
    // waiting on a run it is about to await anyway.
    let (handle_tx, mut handle_rx) = tokio::sync::mpsc::channel(1);
    let _mailbox = tokio::spawn({
        let task_id = task.id.clone();
        async move {
            if let Some(handle) = handle_rx.recv().await {
                scheduler().register_mailbox(&task_id, handle);
            }
        }
    });
    let result = crate::app::run_non_interactive_with(
        config,
        std::path::PathBuf::from(&task.project_path),
        task.model_id.clone(),
        prompt,
        crate::app::RunOptions {
            task_id: Some(task.id.clone()),
            cancel: Some(token.clone()),
            deadline: sched.task_timeout,
            event_tx: Some(event_tx),
            handle_tx: Some(handle_tx),
            ..crate::app::RunOptions::default()
        },
    )
    .await;

    link_completed_session(&task, &result);

    // The run has ended; `_running_guard` removes the token from the running map
    // when this function returns. Map the outcome to a terminal status + report
    // — an explicit cancel wins over whatever the interrupted run returned.
    let (status, report, hook_status) = classify_run_result(token.is_cancelled(), result);
    // F20: persist the terminal status DURABLY (see persist_terminal_status).
    persist_terminal_status(&task.id, status, &report).await;
    // AFTER the terminal status persists: late subscribers now read the row
    // and synthesize a terminal event instead of racing the live stream.
    sched.drop_mailbox(&task.id);
    sched.drop_stream(&task.id);
    record_terminal_outcome(&task, status);
    let _ = mermaid_runtime::run_plugin_hooks(
        "task_stop",
        &serde_json::json!({
            "id": task.id.clone(),
            "status": hook_status,
            "final_report": report.clone(),
        }),
    );
}

pub(super) fn link_completed_session(
    task: &mermaid_runtime::TaskRecord,
    result: &Result<crate::app::RunResult, anyhow::Error>,
) {
    let session_id = result
        .as_ref()
        .ok()
        .map(|run| run.session_id.clone())
        .filter(|id| !id.is_empty());
    if let Some(session_id) = session_id {
        // Only the task row needs stamping: the session's own index row was
        // upserted by the run's saves.
        let task_id = task.id.clone();
        let _ = mermaid_runtime::with_shared_store(move |store| {
            store.tasks().set_conversation(&task_id, &session_id)
        });
    }
}

/// Wait for the run's `session_started` line and write `conversation_id`
/// onto the task row.
///
/// Returns as soon as it has stamped, and when the run's senders drop
/// without ever announcing (a run that failed before it had a session).
/// Best-effort: a store write that fails leaves the row for the end-of-run
/// stamp, and the only cost is a catch-up-less attach in the meantime.
pub(super) async fn early_backlink(
    mut events: tokio::sync::broadcast::Receiver<mermaid_domain::RunEvent>,
    task_id: String,
) {
    loop {
        match events.recv().await {
            Ok(mermaid_domain::RunEvent::SessionStarted { session_id, .. }) => {
                if session_id.is_empty() {
                    return;
                }
                let owned = task_id.clone();
                if let Err(error) = mermaid_runtime::with_shared_store(move |store| {
                    store.tasks().set_conversation(&owned, &session_id)
                }) {
                    tracing::warn!(task = %task_id, %error, "could not stamp the session backlink at run start");
                }
                return;
            },
            // Everything else on this stream is content; keep waiting for
            // the identity line (which the driver sends first, so this is
            // only reached if a lag marker beat it).
            Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {},
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// Durably persist a task's terminal status + report (F20). The daemon's spawned
/// run task is the only writer of this final state; if the write is lost the task
/// is left `running` and the next startup reconcile fails it (discarding the real
/// report). Retry a few times, reopening the store each attempt, and log loudly
/// if it still can't be written rather than swallowing the error.
pub(super) async fn persist_terminal_status(
    task_id: &str,
    status: mermaid_runtime::TaskStatus,
    report: &str,
) {
    const MAX_ATTEMPTS: usize = 3;
    for attempt in 1..=MAX_ATTEMPTS {
        match mermaid_runtime::RuntimeStore::open_default() {
            Ok(store) => match store.tasks().update_status(task_id, status, Some(report)) {
                Ok(()) => return,
                Err(error) => tracing::error!(
                    task_id,
                    attempt,
                    error = %error,
                    "failed to persist terminal task status; retrying"
                ),
            },
            Err(error) => tracing::error!(
                task_id,
                attempt,
                error = %error,
                "failed to open store to persist terminal task status; retrying"
            ),
        }
    }
    tracing::error!(
        task_id,
        "gave up persisting terminal task status after retries; it may be reconciled as failed on the next daemon restart"
    );
}

/// Map a finished run to its terminal `(status, report, hook_status)`. An
/// explicit cancel wins over whatever the interrupted run returned. Otherwise a
/// run with no errors AND a non-empty response is a success; a run with no
/// errors but an EMPTY response produced nothing — for a headless task that is a
/// failure, not a success. Recording the empty case as `Completed` would stamp a
/// false `task_terminal` success/1.0 into the `outcomes` training corpus (the
/// signal the self-improving loop learns from), so it is mapped to `Failed`.
pub(super) fn classify_run_result<E: std::fmt::Display>(
    cancelled: bool,
    result: std::result::Result<crate::app::RunResult, E>,
) -> (mermaid_runtime::TaskStatus, String, &'static str) {
    use mermaid_runtime::TaskStatus;
    if cancelled {
        return (
            TaskStatus::Cancelled,
            "cancelled by user".to_string(),
            "cancelled",
        );
    }
    match result {
        Ok(run) if run.errors.is_empty() && !run.response.trim().is_empty() => {
            (TaskStatus::Completed, run.response, "completed")
        },
        Ok(run) if run.errors.is_empty() => (
            TaskStatus::Failed,
            "model returned an empty response".to_string(),
            "failed",
        ),
        Ok(run) => (TaskStatus::Failed, run.errors.join("\n"), "failed"),
        Err(err) => (TaskStatus::Failed, err.to_string(), "failed"),
    }
}

/// Record a coarse `task_terminal` outcome for a finished daemon run — the
/// cheapest reward signal available today (did the whole trajectory succeed?).
/// Best-effort: a lost outcome must never fail the run, so failures are logged,
/// not propagated. Finer, higher-value signals (test/build results, git-survival,
/// user edit/accept preference pairs) attach to the same `outcomes` table as the
/// run lifecycle grows hooks for them.
pub(super) fn record_terminal_outcome(
    task: &mermaid_runtime::TaskRecord,
    status: mermaid_runtime::TaskStatus,
) {
    use mermaid_runtime::{
        NewOutcome, OUTCOME_LABEL_FAILURE, OUTCOME_LABEL_SUCCESS, OUTCOME_LABEL_UNKNOWN,
        OUTCOME_SOURCE_SYSTEM, RuntimeStore, TaskStatus,
    };
    let (label, reward) = match status {
        TaskStatus::Completed => (OUTCOME_LABEL_SUCCESS, 1.0),
        TaskStatus::Failed => (OUTCOME_LABEL_FAILURE, -1.0),
        // Only Completed/Failed reach here from the run mapper; be explicit
        // rather than silently skip anything else.
        _ => (OUTCOME_LABEL_UNKNOWN, 0.0),
    };
    // Denormalize the task's training context into the outcome so it stays a
    // usable example after the `tasks` row is pruned on the shorter GC window
    // (`outcomes.task_id` is `ON DELETE SET NULL`, so the link is lost then).
    let detail_json = serde_json::to_string(&serde_json::json!({
        "prompt": task.prompt,
        "model_id": task.model_id,
        "conversation_id": task.conversation_id,
        "label": label,
    }))
    .ok();
    let store = match RuntimeStore::open_default() {
        Ok(store) => store,
        Err(error) => {
            tracing::warn!(task_id = %task.id, error = %error, "failed to open store to record terminal outcome");
            return;
        },
    };
    if let Err(error) = store.outcomes().record(NewOutcome {
        id: None,
        task_id: Some(task.id.clone()),
        tool_run_id: None,
        kind: "task_terminal".to_string(),
        label: label.to_string(),
        reward: Some(reward),
        source: OUTCOME_SOURCE_SYSTEM.to_string(),
        detail_json,
    }) {
        tracing::warn!(task_id = %task.id, error = %error, "failed to record terminal outcome");
    }
}

pub(super) fn task_title_from_prompt(prompt: &str) -> String {
    let one_line = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.is_empty() {
        return "daemon task".to_string();
    }
    if one_line.len() <= 80 {
        return one_line;
    }
    let end = one_line.floor_char_boundary(80);
    format!("{}...", &one_line[..end])
}
