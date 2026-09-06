//! Streaming a task's events to an attached client, including the catch-up
//! replay of what the attach missed.

use anyhow::Result;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

use super::*;

/// The terminal `RunEvent` a LATE subscriber gets: the task already ended,
/// so there is no live stream to join and the record has to answer for it.
///
/// A late attach used to be handed a blank `session_id` and zero tokens —
/// the one field a follow-up `mermaid run --resume` needs was the one left
/// empty. Both now come off the task's conversation backlink and the
/// session index row it points at.
pub(super) fn terminal_result_event(
    store: &mermaid_runtime::RuntimeStore,
    task: &mermaid_runtime::TaskRecord,
) -> mermaid_domain::RunEvent {
    let errors = match task.status {
        mermaid_runtime::TaskStatus::Completed => Vec::new(),
        status => vec![format!("task ended {status}")],
    };
    let session_id = task.conversation_id.clone().unwrap_or_default();
    let total_tokens = (!session_id.is_empty())
        .then(|| store.sessions().get(&session_id).ok().flatten())
        .flatten()
        .and_then(|session| session.total_tokens)
        .and_then(|total| u64::try_from(total).ok())
        .unwrap_or(0);
    mermaid_domain::RunEvent::Result {
        response: task.final_report.clone().unwrap_or_default(),
        reasoning: None,
        total_tokens,
        errors,
        session_id,
        structured_output: None,
    }
}

/// How many catch-up events one attach replays. The whole projection is
/// built in memory and written to a single socket before the first live
/// event can flow, so a long run's log needs a ceiling — the same reflex as
/// F24/RC-F on the transcript read. Tail-first: a subscriber joining now
/// wants the recent end.
pub(super) const MAX_CATCH_UP_EVENTS: usize = 1_000;

/// What a `subscribe_task` attach MISSED: the run's `session_started` line
/// plus the committed transcript so far, projected onto the public stream.
///
/// The live broadcast carries only what happens from now, so a subscriber
/// attaching at minute nine used to get nine minutes of silence and then
/// whatever came next — not even the `session_started` line that names the
/// session, the same empty-handed attach the terminal path was fixed for in
/// #371. The session event log is the durable record of everything before
/// the attach, and `tasks.conversation_id` — stamped when the run announces
/// its session, not just at terminal status — is the key to it.
///
/// A daemon task never seeds a session (the scheduler passes no
/// `RunOptions::seed`), so its log holds this run and nothing else: the
/// catch-up cannot leak an unrelated conversation's history.
///
/// Best-effort by construction. No backlink yet (a queued task, or a run
/// that has not announced), no log, a project directory that has since gone
/// away, or an unreadable log all yield what is known rather than failing
/// the subscription.
pub(super) fn catch_up_events(task: &mermaid_runtime::TaskRecord) -> Vec<mermaid_domain::RunEvent> {
    let Some(session_id) = task.conversation_id.as_deref().filter(|id| !id.is_empty()) else {
        return Vec::new();
    };
    // Identity is stated from the TASK row, not the log's `started` event:
    // this is the line the live driver emitted for THIS run, and the row is
    // what knows the task it belongs to.
    let started = mermaid_domain::RunEvent::SessionStarted {
        protocol_version: mermaid_domain::RUN_EVENT_PROTOCOL_VERSION,
        cli_version: env!("CARGO_PKG_VERSION").to_string(),
        model: task.model_id.clone(),
        task_id: Some(task.id.clone()),
        session_id: session_id.to_string(),
    };
    // `ConversationManager::new` CREATES the conversations dir. A task whose
    // project has been deleted must not resurrect it just because somebody
    // subscribed, so check before constructing one.
    if !std::path::Path::new(&task.project_path).is_dir() {
        return vec![started];
    }
    let events = match crate::session::ConversationManager::new(&task.project_path)
        .and_then(|manager| manager.read_session_events(session_id))
    {
        Ok(Some(events)) => events,
        Ok(None) => return vec![started],
        Err(error) => {
            tracing::warn!(task = %task.id, %error, "session log unreadable; attaching without catch-up");
            return vec![started];
        },
    };
    let mut replay = mermaid_domain::RunEvent::catch_up(&events);
    if let Some(dropped) = replay
        .len()
        .checked_sub(MAX_CATCH_UP_EVENTS)
        .filter(|n| *n > 0)
    {
        tracing::info!(task = %task.id, dropped, "catch-up over the cap; replaying the tail");
        replay.drain(..dropped);
    }
    std::iter::once(started).chain(replay).collect()
}

/// Serve one `subscribe_task` connection: ack line, the catch-up the attach
/// missed, then NDJSON `RunEvent`s until the terminal `result`. An
/// already-terminal task gets ONE synthesized result from the persisted
/// record in place of the live stream. Slow clients are dropped by a
/// per-write timeout so they can't block the daemon.
pub(super) async fn handle_subscribe_stream<S>(
    mut stream: S,
    request: crate::runtime_client::DaemonRequest,
    authorized: bool,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    const WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
    async fn write_line<S: AsyncWrite + Unpin>(stream: &mut S, line: &str) -> Result<()> {
        tokio::time::timeout(WRITE_TIMEOUT, async {
            stream.write_all(line.as_bytes()).await?;
            stream.write_all(b"\n").await?;
            Ok::<(), std::io::Error>(())
        })
        .await
        .map_err(|_| anyhow::anyhow!("subscriber too slow; dropping connection"))??;
        Ok(())
    }

    let crate::runtime_client::DaemonRequest::SubscribeTask { task_id } = request else {
        anyhow::bail!("handle_subscribe_stream called with a non-subscribe request");
    };
    if !authorized {
        write_line(
            &mut stream,
            &serde_json::json!({
                "ok": false,
                "error": "unauthorized: set MERMAID_DAEMON_TOKEN or include auth.token",
            })
            .to_string(),
        )
        .await?;
        return Ok(());
    }
    let store = mermaid_runtime::RuntimeStore::open_default()?;
    let Some(task) = store.tasks().get(&task_id)? else {
        write_line(
            &mut stream,
            &serde_json::json!({"ok": false, "error": format!("task not found: {task_id}")})
                .to_string(),
        )
        .await?;
        return Ok(());
    };
    // Attach the receiver FIRST, then read the status: if the task went
    // terminal between the two, we synthesize — no event window is lost
    // (the executor drops the stream only AFTER persisting the status).
    let mut rx = scheduler().stream_for(&task_id).subscribe();
    let task = store.tasks().get(&task_id)?.unwrap_or(task);
    // Read the log AFTER attaching the receiver, in that order on purpose.
    // A message committed while we read is then replayed AND delivered live
    // — a duplicate bounded to the one in-flight message — where reading
    // first would drop it from both. Repetition is recoverable by a
    // consumer; a hole is not.
    //
    // On a blocking pool: the log is the one unbounded read on this path,
    // and a multi-megabyte transcript must not stall the reactor for every
    // other connection. A panicking read costs the catch-up, not the
    // subscription.
    let catch_up = {
        let task = task.clone();
        tokio::task::spawn_blocking(move || catch_up_events(&task))
            .await
            .unwrap_or_default()
    };
    write_line(
        &mut stream,
        &serde_json::json!({
            "ok": true,
            "subscribed": task_id,
            "status": task.status.to_string(),
            "protocol_version": mermaid_domain::RUN_EVENT_PROTOCOL_VERSION,
            // How many of the lines that follow are REPLAYED from the log
            // rather than live. A consumer that only wants what happens
            // from now skips exactly this many.
            "replayed": catch_up.len(),
        })
        .to_string(),
    )
    .await?;
    for event in &catch_up {
        write_line(&mut stream, &serde_json::to_string(event)?).await?;
    }
    let terminal_status = matches!(
        task.status,
        mermaid_runtime::TaskStatus::Completed
            | mermaid_runtime::TaskStatus::Failed
            | mermaid_runtime::TaskStatus::Cancelled
    );
    if terminal_status {
        let event = terminal_result_event(&store, &task);
        write_line(&mut stream, &serde_json::to_string(&event)?).await?;
        stream.shutdown().await?;
        return Ok(());
    }
    loop {
        match rx.recv().await {
            Ok(event) => {
                let terminal = matches!(event, mermaid_domain::RunEvent::Result { .. });
                write_line(&mut stream, &serde_json::to_string(&event)?).await?;
                if terminal {
                    break;
                }
            },
            // Lagged: stay inside the RunEvent contract with an error event.
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                let event = mermaid_domain::RunEvent::Error {
                    message: format!("subscriber lagged; {n} events dropped"),
                };
                write_line(&mut stream, &serde_json::to_string(&event)?).await?;
            },
            // Sender dropped without a Result (daemon shutdown mid-run).
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }
    stream.shutdown().await?;
    Ok(())
}
