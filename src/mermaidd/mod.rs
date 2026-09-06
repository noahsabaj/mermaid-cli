//! `mermaidd`, the daemon, as a library: the scheduler, the listeners, the
//! subscribe stream and the JSON API. The binary in `src/bin/mermaidd.rs` is
//! a thin entry point over [`run`]. Everything a server does used to live in
//! that binary, where nothing could reuse or unit-test it.

use anyhow::Result;

mod api;
pub mod cli;
mod recovery;
mod scheduler;
mod server;
mod subscribe;

#[cfg(test)]
use cli::{CliAction, classify_args};
#[cfg(test)]
use recovery::sweep_stale_bg_logs_in;
use scheduler::{SCHEDULER, Scheduler};
#[cfg(test)]
use scheduler::{classify_run_result, early_backlink, scheduler};
#[cfg(test)]
use server::parse_subscribe;
#[cfg(unix)]
use server::serve_unix;
#[cfg(windows)]
use server::serve_windows;
#[cfg(all(test, unix))]
use server::uid_allowed;
#[cfg(test)]
use subscribe::{catch_up_events, handle_subscribe_stream};

/// Start the daemon and serve until it is stopped.
///
/// # Errors
///
/// Errors if the runtime store cannot be opened, or the listener cannot bind.
pub async fn run() -> Result<()> {
    // Open (and thereby create/validate) the runtime store up front on every
    // platform: a broken DB should fail the daemon fast, not its first client.
    drop(mermaid_runtime::RuntimeStore::open_default()?);

    // Scheduler singleton. Config is read once at startup (a restart picks up
    // changes); the drain loop itself is spawned by the serve fns AFTER the
    // platform singleton guard + startup_recovery, so a fresh claim can never
    // race the stranded-Running reconcile.
    let daemon_config = crate::app::load_config().unwrap_or_default().daemon;
    let _ = SCHEDULER.set(Scheduler {
        permits: std::sync::Arc::new(tokio::sync::Semaphore::new(
            daemon_config.max_concurrent_tasks.max(1),
        )),
        running: std::sync::Mutex::new(std::collections::HashMap::new()),
        streams: std::sync::Mutex::new(std::collections::HashMap::new()),
        mailboxes: std::sync::Mutex::new(std::collections::HashMap::new()),
        wake: tokio::sync::Notify::new(),
        task_timeout: daemon_config
            .task_timeout_minutes
            .map(|minutes| std::time::Duration::from_secs(minutes * 60)),
    });

    #[cfg(unix)]
    return serve_unix().await;
    #[cfg(windows)]
    return serve_windows().await;
}

#[cfg(test)]
mod tests {
    #[test]
    fn classify_args_handles_flags_help_and_unknowns() {
        use super::{CliAction, classify_args};
        assert_eq!(classify_args(Vec::<String>::new()), CliAction::Run);
        for v in ["--version", "-V", "version"] {
            assert_eq!(classify_args([v.to_string()]), CliAction::Version, "{v}");
        }
        for h in ["--help", "-h", "help"] {
            assert_eq!(classify_args([h.to_string()]), CliAction::Help, "{h}");
        }
        assert_eq!(
            classify_args(["--bogus".to_string()]),
            CliAction::Unknown("--bogus".to_string())
        );
    }

    #[test]
    fn classify_run_result_maps_empty_response_to_failure() {
        use super::classify_run_result;
        use crate::app::RunResult;
        use mermaid_runtime::TaskStatus;

        // A real response with no errors → success.
        let (status, report, hook) = classify_run_result::<String>(
            false,
            Ok(RunResult {
                response: "here is the answer".to_string(),
                ..Default::default()
            }),
        );
        assert_eq!(status, TaskStatus::Completed);
        assert_eq!(report, "here is the answer");
        assert_eq!(hook, "completed");

        // No errors but an EMPTY (whitespace-only) response → failure, NOT a
        // false success/1.0 in the outcomes signal.
        let (status, report, hook) = classify_run_result::<String>(
            false,
            Ok(RunResult {
                response: "   \n".to_string(),
                ..Default::default()
            }),
        );
        assert_eq!(status, TaskStatus::Failed);
        assert_eq!(report, "model returned an empty response");
        assert_eq!(hook, "failed");

        // Tool/action errors → failure carrying the joined errors.
        let (status, report, _) = classify_run_result::<String>(
            false,
            Ok(RunResult {
                errors: vec!["exec: boom".to_string()],
                ..Default::default()
            }),
        );
        assert_eq!(status, TaskStatus::Failed);
        assert_eq!(report, "exec: boom");

        // A run error → failure carrying the error text.
        let (status, report, _) =
            classify_run_result(false, Err::<RunResult, _>("provider exploded"));
        assert_eq!(status, TaskStatus::Failed);
        assert_eq!(report, "provider exploded");

        // An explicit cancel wins over the run result, even a good one.
        let (status, _, hook) = classify_run_result::<String>(
            true,
            Ok(RunResult {
                response: "ignored".to_string(),
                ..Default::default()
            }),
        );
        assert_eq!(status, TaskStatus::Cancelled);
        assert_eq!(hook, "cancelled");
    }

    #[test]
    fn sweep_stale_bg_logs_targets_only_old_bg_logs() {
        let dir = std::env::temp_dir().join(format!("mermaidd_bg_sweep_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bg = dir.join("mermaid-bg-1234-99.log");
        std::fs::write(&bg, b"old log").unwrap();
        let keep = dir.join("notes.txt");
        std::fs::write(&keep, b"keep me").unwrap();
        let other_log = dir.join("mermaidd.log");
        std::fs::write(&other_log, b"daemon log").unwrap();

        // retention 0 → the cutoff is "now", captured after the files were
        // written, so the just-created bg log counts as stale and is reaped;
        // non-matching names survive.
        let removed = super::sweep_stale_bg_logs_in(&dir, 0).expect("sweep");
        assert_eq!(removed, 1);
        assert!(!bg.exists(), "the bg tee log must be reaped");
        assert!(keep.exists(), "unrelated files must survive");
        assert!(other_log.exists(), "non-bg logs must survive");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scratchpad_sweep_honors_the_lock_and_the_daemon_retention() {
        // The daemon's startup sweep is `scratchpad::sweep_stale` over the
        // knob; drive its `_in` seam against a fixture root the same way the
        // bg-log test does. Layout: <root>/<project-slug>/<session-id>.
        let root =
            std::env::temp_dir().join(format!("mermaidd_scratch_sweep_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let abandoned = root.join("-proj").join("abandoned");
        std::fs::create_dir_all(abandoned.join("scratchpad")).unwrap();
        std::fs::write(abandoned.join("scratchpad").join("out.txt"), b"stale").unwrap();
        let live = root.join("-proj").join("live");
        std::fs::create_dir_all(live.join("scratchpad")).unwrap();
        // A held flock = a live owner; never reaped. Hold it for the test's
        // duration the same way a running mermaid holds it for its lifetime.
        let lock = std::fs::File::create(live.join(".lock")).unwrap();
        lock.try_lock().expect("acquire test lock");

        // Retention 0 (the daemon knob clamped) → age never protects; only
        // the held lock does.
        let removed = crate::session::scratchpad::sweep_stale_in(&root, 0).expect("sweep");
        assert_eq!(removed, 1);
        assert!(!abandoned.exists(), "unheld session dirs are reaped");
        assert!(live.exists(), "a held lock protects the session dir");
        drop(lock);

        let _ = std::fs::remove_dir_all(&root);
    }

    fn temp_db(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("mermaidd_runtime_hygiene_{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir.join("runtime.sqlite3")
    }

    #[test]
    fn mutating_json_commands_require_auth_on_local_socket() {
        use crate::runtime_client::DaemonRequest;
        // The historical stringly matrix, now driven by the exhaustive typed
        // one. Wire strings parse into the enum and their gating matches.
        let gated = [
            r#"{"command":"create_task","title":"t","project_path":"p","model_id":"m"}"#,
            r#"{"command":"run","prompt":"p"}"#,
            r#"{"command":"cancel_task","id":"t"}"#,
            r#"{"command":"send_to_task","id":"t","text":"hi"}"#,
            r#"{"command":"update_task","id":"t","status":"completed"}"#,
            r#"{"command":"restore_checkpoint","id":"c"}"#,
            r#"{"command":"approve","id":"a"}"#,
            r#"{"command":"deny","id":"a"}"#,
            r#"{"command":"stop_process","id":"p"}"#,
            r#"{"command":"restart_process","id":"p"}"#,
            r#"{"command":"open_process","id":"p"}"#,
            r#"{"command":"plugin_preview","path":"/p"}"#,
            r#"{"command":"plugin_install","path":"/p"}"#,
            r#"{"command":"set_plugin_enabled","id":"p","enabled":true}"#,
            r#"{"command":"set_safety_mode","mode":"ask"}"#,
            r#"{"command":"runtime_hygiene_archive"}"#,
            r#"{"command":"pair"}"#,
            r#"{"command":"logs","id":"p"}"#,
            // #21: privileged reads gated behind the pairing token too.
            r#"{"command":"session_messages","id":"s"}"#,
            r#"{"command":"snapshot"}"#,
            r#"{"command":"runtime_snapshot"}"#,
            r#"{"command":"runtime_dashboard"}"#,
            r#"{"command":"runtime_diagnostics"}"#,
            r#"{"command":"runtime_hygiene_preview"}"#,
            r#"{"command":"runtime_task_detail","id":"t"}"#,
            r#"{"command":"runtime_approval_detail","id":"a"}"#,
            r#"{"command":"runtime_checkpoint_detail","id":"c"}"#,
            r#"{"command":"runtime_tasks"}"#,
            r#"{"command":"runtime_processes"}"#,
            r#"{"command":"runtime_approvals"}"#,
            r#"{"command":"runtime_tool_runs"}"#,
            r#"{"command":"runtime_checkpoints"}"#,
            r#"{"command":"runtime_plugins"}"#,
            r#"{"command":"model_info","model":"m"}"#,
            // Session content flows through the stream: gated.
            r#"{"command":"subscribe_task","task_id":"t"}"#,
        ];
        for wire in gated {
            let req: DaemonRequest = serde_json::from_str(wire).expect(wire);
            assert!(req.requires_auth(), "{wire}");
        }
        // Liveness/discovery stay unauthenticated on the local socket.
        for wire in [r#"{"command":"health"}"#, r#"{"command":"ports"}"#] {
            let req: DaemonRequest = serde_json::from_str(wire).expect(wire);
            assert!(!req.requires_auth(), "{wire}");
        }
    }

    /// The plaintext-unknown help list and the typed enum must agree — a new
    /// variant without a help entry (or vice versa) fails here.
    #[test]
    fn help_list_matches_the_typed_command_set() {
        let help = [
            "create_task",
            "run",
            "cancel_task",
            "send_to_task",
            "update_task",
            "session_messages",
            "snapshot",
            "runtime_snapshot",
            "runtime_dashboard",
            "runtime_diagnostics",
            "runtime_hygiene_preview",
            "runtime_hygiene_archive",
            "runtime_task_detail",
            "runtime_approval_detail",
            "runtime_checkpoint_detail",
            "runtime_tasks",
            "runtime_processes",
            "runtime_approvals",
            "runtime_tool_runs",
            "runtime_checkpoints",
            "runtime_plugins",
            "logs",
            "stop_process",
            "restart_process",
            "open_process",
            "ports",
            "restore_checkpoint",
            "approve",
            "deny",
            "plugin_preview",
            "plugin_install",
            "set_plugin_enabled",
            "model_info",
            "set_safety_mode",
            "pair",
            "subscribe_task",
        ];
        // Every help entry must parse as a typed command (given minimal args).
        for name in help {
            let mut body = serde_json::json!({
                "command": name,
                "title": "t", "project_path": "p", "model_id": "m", "prompt": "p",
                "id": "x", "status": "completed", "path": "/p", "enabled": true,
                "mode": "ask", "model": "m", "task_id": "t", "text": "hi",
            });
            body.as_object_mut().unwrap().retain(|_, v| !v.is_null());
            let parsed = serde_json::from_value::<crate::runtime_client::DaemonRequest>(body);
            assert!(
                parsed.is_ok(),
                "help entry '{name}' no longer parses: {parsed:?}"
            );
        }
    }

    fn bare_scheduler() -> super::Scheduler {
        super::Scheduler {
            permits: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
            running: std::sync::Mutex::new(std::collections::HashMap::new()),
            wake: tokio::sync::Notify::new(),
            task_timeout: None,
            streams: std::sync::Mutex::new(std::collections::HashMap::new()),
            mailboxes: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Unlike `streams`, a mailbox is never created on demand: a queued task
    /// has no reducer to talk to, and inventing an inbox nobody reads would
    /// turn "not running" into a message that vanishes.
    #[tokio::test]
    async fn a_mailbox_exists_only_while_its_task_runs() {
        let sched = bare_scheduler();
        assert!(
            sched.mailbox_for("t1").is_none(),
            "no mailbox before the run publishes one"
        );

        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        sched.register_mailbox(
            "t1",
            crate::engine::EngineHandle::new(tx, sched.stream_for("t1")),
        );

        sched
            .mailbox_for("t1")
            .expect("registered")
            .try_send(mermaid_domain::Msg::SubmitPrompt {
                text: "also check the tests".to_string(),
                attachment_ids: vec![],
            })
            .expect("the run is listening");
        let delivered = rx.try_recv().expect("delivered to the run's own inbox");
        assert!(
            matches!(&delivered, mermaid_domain::Msg::SubmitPrompt { text, .. } if text == "also check the tests"),
            "wrong message: {delivered:?}"
        );

        // The run ends: the drop guard clears the entry, so a later send is
        // told the task is not running rather than being silently dropped.
        sched.drop_mailbox("t1");
        assert!(sched.mailbox_for("t1").is_none());
    }

    #[test]
    fn stream_registry_is_get_or_create_and_drop() {
        let sched = super::Scheduler {
            permits: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
            running: std::sync::Mutex::new(std::collections::HashMap::new()),
            wake: tokio::sync::Notify::new(),
            task_timeout: None,
            streams: std::sync::Mutex::new(std::collections::HashMap::new()),
            mailboxes: std::sync::Mutex::new(std::collections::HashMap::new()),
        };
        // Subscribe BEFORE the executor asks: both get the same sender, so a
        // pre-run subscriber receives the run's events.
        let early = sched.stream_for("t1");
        let mut rx = early.subscribe();
        let executor_side = sched.stream_for("t1");
        executor_side
            .send(mermaid_domain::RunEvent::Error {
                message: "hello".to_string(),
            })
            .expect("subscriber attached");
        match rx.try_recv().expect("event delivered") {
            mermaid_domain::RunEvent::Error { message } => assert_eq!(message, "hello"),
            other => panic!("wrong event: {other:?}"),
        }
        // Drop guard cleans the entry; the next stream_for is a fresh channel.
        sched.drop_stream("t1");
        assert!(sched.streams.lock().unwrap().is_empty());
    }

    /// A task row pointing at `session` under `project`, mid-run.
    fn running_task(
        project: &std::path::Path,
        session: Option<&str>,
    ) -> mermaid_runtime::TaskRecord {
        mermaid_runtime::TaskRecord {
            id: "task-1".to_string(),
            title: "t".to_string(),
            status: mermaid_runtime::TaskStatus::Running,
            priority: mermaid_runtime::TaskPriority::Normal,
            project_path: project.display().to_string(),
            model_id: "ollama/test".to_string(),
            conversation_id: session.map(str::to_string),
            created_at: "2026-08-10T00:00:00-04:00".to_string(),
            updated_at: "2026-08-10T00:00:00-04:00".to_string(),
            final_report: None,
            prompt: Some("p".to_string()),
        }
    }

    /// Write a real session log the way a run does — through the appender,
    /// which backfills the transcript on first touch.
    fn seeded_session(project: &std::path::Path, assistant_says: &str) -> String {
        let manager =
            crate::session::ConversationManager::new(project).expect("conversation manager");
        let mut conversation = mermaid_domain::ConversationHistory::new(
            project.display().to_string(),
            "ollama/test".to_string(),
            chrono::Local::now(),
        );
        // `vec!` and not an array literal: two `ChatMessage`s together clear
        // the 512-byte stack-array threshold, and this is a test helper, not
        // a place to spend debt.
        let turns = vec![
            mermaid_model::models::ChatMessage::user("do the thing"),
            mermaid_model::models::ChatMessage::assistant(assistant_says),
        ];
        conversation.add_messages(&turns, chrono::Local::now());
        manager
            .append_session_events(&conversation, &[])
            .expect("append the log");
        conversation.id
    }

    fn temp_project(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("mermaidd_catch_up_{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp project");
        dir
    }

    /// A mid-run attach is served the identity line it missed plus the
    /// transcript committed before it arrived — the whole point of the
    /// catch-up.
    #[test]
    fn catch_up_replays_identity_and_the_transcript_so_far() {
        let project = temp_project("transcript");
        let session = seeded_session(&project, "half way there");
        let events = super::catch_up_events(&running_task(&project, Some(&session)));

        assert_eq!(
            events[0],
            mermaid_domain::RunEvent::SessionStarted {
                protocol_version: mermaid_domain::RUN_EVENT_PROTOCOL_VERSION,
                cli_version: env!("CARGO_PKG_VERSION").to_string(),
                model: "ollama/test".to_string(),
                task_id: Some("task-1".to_string()),
                session_id: session,
            },
            "identity leads the catch-up: {events:?}"
        );
        assert_eq!(
            events[1],
            mermaid_domain::RunEvent::Text {
                delta: "half way there".to_string()
            },
            "the committed assistant turn replays: {events:?}"
        );
        let _ = std::fs::remove_dir_all(&project);
    }

    /// No backlink yet (a still-queued task) means nothing to replay — the
    /// subscription is live-only, exactly as before.
    #[test]
    fn catch_up_is_empty_without_a_session_backlink() {
        let project = temp_project("no_backlink");
        assert!(super::catch_up_events(&running_task(&project, None)).is_empty());
        let _ = std::fs::remove_dir_all(&project);
    }

    /// A task whose project has been deleted still gets its identity line,
    /// and subscribing must not recreate the directory tree to find that out.
    #[test]
    fn catch_up_does_not_resurrect_a_deleted_project() {
        let project = temp_project("deleted");
        let session = seeded_session(&project, "gone now");
        std::fs::remove_dir_all(&project).expect("delete the project");

        let events = super::catch_up_events(&running_task(&project, Some(&session)));

        assert_eq!(events.len(), 1, "identity only: {events:?}");
        assert!(matches!(
            events[0],
            mermaid_domain::RunEvent::SessionStarted { .. }
        ));
        assert!(!project.exists(), "the project dir stays deleted");
    }

    /// The backlink that makes a mid-run catch-up possible at all: the
    /// watcher stamps `conversation_id` off the run's own `session_started`
    /// line, long before the terminal status that used to be the only writer.
    #[tokio::test]
    async fn the_backlink_lands_when_the_run_announces_its_session() {
        let data_dir = temp_project("backlink_data");
        temp_env::async_with_vars(
            [(
                mermaid_model::utils::DATA_DIR_ENV,
                Some(data_dir.display().to_string()),
            )],
            async {
                let store = mermaid_runtime::RuntimeStore::open_default().expect("open store");
                let task = store
                    .tasks()
                    .create(
                        mermaid_runtime::NewTask::new("t", "/tmp/proj", "ollama/test")
                            .daemon_owned()
                            .with_prompt("do the thing"),
                    )
                    .expect("create task");
                assert!(
                    task.conversation_id.is_none(),
                    "a task is created before its session exists"
                );

                let (events, _rx) = tokio::sync::broadcast::channel(16);
                let watcher =
                    tokio::spawn(super::early_backlink(events.subscribe(), task.id.clone()));
                events
                    .send(mermaid_domain::RunEvent::SessionStarted {
                        protocol_version: mermaid_domain::RUN_EVENT_PROTOCOL_VERSION,
                        cli_version: env!("CARGO_PKG_VERSION").to_string(),
                        model: "ollama/test".to_string(),
                        task_id: Some(task.id.clone()),
                        session_id: "20260810_120000_000".to_string(),
                    })
                    .expect("the watcher is subscribed");
                tokio::time::timeout(std::time::Duration::from_secs(20), watcher)
                    .await
                    .expect("the watcher stamps and returns")
                    .expect("watcher joined");

                let stamped = mermaid_runtime::RuntimeStore::open_default()
                    .expect("reopen store")
                    .tasks()
                    .get(&task.id)
                    .expect("read the task")
                    .expect("the task exists");
                assert_eq!(
                    stamped.conversation_id.as_deref(),
                    Some("20260810_120000_000"),
                    "the session is reachable from the task while the run is still going"
                );
            },
        )
        .await;
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    /// End to end over a real connection: a subscriber attaching to a
    /// RUNNING task reads the ack, then the catch-up, then the live stream —
    /// in that order, on one socket.
    ///
    /// This is the behavior the projection tests above cannot show: that a
    /// mid-run attach is served what it missed BEFORE it joins the
    /// broadcast, and that the ack says how many of the lines that follow
    /// were replayed rather than live.
    #[tokio::test]
    async fn a_mid_run_attach_reads_the_catch_up_then_the_live_stream() {
        use tokio::io::{AsyncBufReadExt, BufReader};

        let data_dir = temp_project("stream_data");
        let project = temp_project("stream_project");
        let session = seeded_session(&project, "half way there");
        let _ = super::SCHEDULER.set(super::Scheduler {
            permits: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
            running: std::sync::Mutex::new(std::collections::HashMap::new()),
            wake: tokio::sync::Notify::new(),
            task_timeout: None,
            streams: std::sync::Mutex::new(std::collections::HashMap::new()),
            mailboxes: std::sync::Mutex::new(std::collections::HashMap::new()),
        });

        let lines = temp_env::async_with_vars(
            [(
                mermaid_model::utils::DATA_DIR_ENV,
                Some(data_dir.display().to_string()),
            )],
            async {
                let store = mermaid_runtime::RuntimeStore::open_default().expect("open store");
                let task = store
                    .tasks()
                    .create(
                        mermaid_runtime::NewTask::new(
                            "t",
                            project.display().to_string(),
                            "ollama/test",
                        )
                        .daemon_owned()
                        .with_prompt("do the thing"),
                    )
                    .expect("create task");
                store
                    .tasks()
                    .update_status(&task.id, mermaid_runtime::TaskStatus::Running, None)
                    .expect("mark running");
                // The backlink the executor stamps when the run announces
                // its session — what makes the log reachable mid-run.
                store
                    .tasks()
                    .set_conversation(&task.id, &session)
                    .expect("stamp the backlink");

                // Hold the sender BEFORE the handler subscribes, so the
                // terminal event below cannot be sent into a void.
                let live = super::scheduler().stream_for(&task.id);
                let (subscriber, socket) = tokio::io::duplex(64 * 1024);
                let handler = tokio::spawn(super::handle_subscribe_stream(
                    socket,
                    crate::runtime_client::DaemonRequest::SubscribeTask {
                        task_id: task.id.clone(),
                    },
                    true,
                ));

                let mut reader = BufReader::new(subscriber).lines();
                let mut lines = Vec::new();
                // Ack + the two catch-up lines. Reading them proves the
                // handler is already past `subscribe()`, so the live event
                // sent next cannot race the attach. Bounded, because a
                // catch-up that never arrives would otherwise hang the
                // suite instead of failing it.
                for _ in 0..3 {
                    lines.push(
                        tokio::time::timeout(
                            std::time::Duration::from_secs(20),
                            reader.next_line(),
                        )
                        .await
                        .expect("the catch-up arrives before the live stream")
                        .expect("read")
                        .expect("a line"),
                    );
                }
                live.send(mermaid_domain::RunEvent::Result {
                    response: "all done".to_string(),
                    reasoning: None,
                    total_tokens: 7,
                    errors: vec![],
                    session_id: session.clone(),
                    structured_output: None,
                })
                .expect("the handler is subscribed");
                while let Ok(Some(line)) = reader.next_line().await {
                    lines.push(line);
                }
                handler.await.expect("handler joined").expect("served");
                lines
            },
        )
        .await;

        let parsed: Vec<serde_json::Value> = lines
            .iter()
            .map(|line| serde_json::from_str(line).expect("each line is JSON"))
            .collect();
        assert_eq!(parsed.len(), 4, "ack + 2 replayed + 1 live: {lines:?}");
        assert_eq!(parsed[0]["ok"], serde_json::json!(true));
        assert_eq!(parsed[0]["status"], serde_json::json!("running"));
        assert_eq!(
            parsed[0]["replayed"],
            serde_json::json!(2),
            "the ack counts the replayed lines that follow"
        );
        assert_eq!(parsed[1]["type"], serde_json::json!("session_started"));
        assert_eq!(parsed[1]["session_id"], serde_json::json!(session));
        assert_eq!(parsed[2]["type"], serde_json::json!("text"));
        assert_eq!(parsed[2]["delta"], serde_json::json!("half way there"));
        // Only after the catch-up does the live broadcast reach the wire.
        assert_eq!(parsed[3]["type"], serde_json::json!("result"));
        assert_eq!(parsed[3]["response"], serde_json::json!("all done"));

        let _ = std::fs::remove_dir_all(&project);
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    /// The rule is uniform: an attach gets what it missed, whenever it
    /// arrives. A LATE one missed everything, so the catch-up precedes the
    /// terminal event synthesized from the record — and the stream still
    /// ends at `result`, so a consumer that stops there is unaffected.
    #[tokio::test]
    async fn a_late_attach_gets_the_catch_up_before_the_synthesized_result() {
        use tokio::io::{AsyncBufReadExt, BufReader};

        let data_dir = temp_project("late_data");
        let project = temp_project("late_project");
        let session = seeded_session(&project, "the finished answer");
        let _ = super::SCHEDULER.set(super::Scheduler {
            permits: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
            running: std::sync::Mutex::new(std::collections::HashMap::new()),
            wake: tokio::sync::Notify::new(),
            task_timeout: None,
            streams: std::sync::Mutex::new(std::collections::HashMap::new()),
            mailboxes: std::sync::Mutex::new(std::collections::HashMap::new()),
        });

        let lines = temp_env::async_with_vars(
            [(
                mermaid_model::utils::DATA_DIR_ENV,
                Some(data_dir.display().to_string()),
            )],
            async {
                let store = mermaid_runtime::RuntimeStore::open_default().expect("open store");
                let task = store
                    .tasks()
                    .create(
                        mermaid_runtime::NewTask::new(
                            "t",
                            project.display().to_string(),
                            "ollama/test",
                        )
                        .daemon_owned()
                        .with_prompt("do the thing"),
                    )
                    .expect("create task");
                store
                    .tasks()
                    .update_status(
                        &task.id,
                        mermaid_runtime::TaskStatus::Completed,
                        Some("the finished answer"),
                    )
                    .expect("mark completed");
                store
                    .tasks()
                    .set_conversation(&task.id, &session)
                    .expect("stamp the backlink");

                let (subscriber, socket) = tokio::io::duplex(64 * 1024);
                let handler = tokio::spawn(super::handle_subscribe_stream(
                    socket,
                    crate::runtime_client::DaemonRequest::SubscribeTask {
                        task_id: task.id.clone(),
                    },
                    true,
                ));
                let mut reader = BufReader::new(subscriber).lines();
                let mut lines = Vec::new();
                while let Ok(Ok(Some(line))) =
                    tokio::time::timeout(std::time::Duration::from_secs(20), reader.next_line())
                        .await
                {
                    lines.push(line);
                }
                handler.await.expect("handler joined").expect("served");
                lines
            },
        )
        .await;

        let parsed: Vec<serde_json::Value> = lines
            .iter()
            .map(|line| serde_json::from_str(line).expect("each line is JSON"))
            .collect();
        assert_eq!(parsed.len(), 4, "ack + 2 replayed + 1 terminal: {lines:?}");
        assert_eq!(parsed[0]["status"], serde_json::json!("completed"));
        assert_eq!(parsed[0]["replayed"], serde_json::json!(2));
        assert_eq!(parsed[1]["type"], serde_json::json!("session_started"));
        assert_eq!(parsed[2]["type"], serde_json::json!("text"));
        assert_eq!(parsed[2]["delta"], serde_json::json!("the finished answer"));
        assert_eq!(parsed[3]["type"], serde_json::json!("result"));
        assert_eq!(parsed[3]["session_id"], serde_json::json!(session));

        let _ = std::fs::remove_dir_all(&project);
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[test]
    fn parse_subscribe_classifies_only_subscribe_task() {
        assert!(super::parse_subscribe(r#"{"command":"subscribe_task","task_id":"t"}"#).is_some());
        assert!(super::parse_subscribe(r#"{"command":"health"}"#).is_none());
        assert!(super::parse_subscribe("health").is_none());
        assert!(super::parse_subscribe(r#"{"command":"nope"}"#).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn uid_allowed_accepts_owner_and_root_only() {
        assert!(super::uid_allowed(1000, 1000), "owner uid must be allowed");
        assert!(super::uid_allowed(0, 1000), "root must be allowed");
        assert!(
            !super::uid_allowed(1001, 1000),
            "a non-owner, non-root uid must be rejected"
        );
    }

    #[test]
    fn runtime_hygiene_preview_matches_test_artifacts_and_archive_is_idempotent() {
        let path = temp_db("preview");
        let store = mermaid_runtime::RuntimeStore::open(&path).expect("open store");
        let checkpoint = store
            .checkpoints()
            .create(mermaid_runtime::NewCheckpoint {
                id: Some("checkpoint-test".to_string()),
                task_id: None,
                project_path: "/tmp/mermaid_checkpoint_test".to_string(),
                snapshot_path: "/data/checkpoints/checkpoint-test".to_string(),
                changed_files_json: "[]".to_string(),
                pending_action_json: Some("{\"tool\":\"write_file\"}".to_string()),
                approval_id: None,
                session_id: None,
                message_index: None,
            })
            .expect("create checkpoint");
        let approval = store
            .approvals()
            .create(mermaid_runtime::NewApproval {
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

        let service = crate::runtime_client::RuntimeService::from_store(store);
        let preview = service.hygiene_preview().expect("preview");
        assert_eq!(preview.counts.approvals, 1);
        assert_eq!(preview.counts.checkpoints, 1);
        let archived = service.hygiene_archive().expect("archive");
        assert_eq!(archived.archived.total, 2);
        let archived_again = service.hygiene_archive().expect("archive again");
        assert_eq!(archived_again.archived.total, 0);
        let store = mermaid_runtime::RuntimeStore::open(&path).expect("reopen store");
        assert!(store.approvals().list_pending().unwrap().is_empty());
        assert!(store.checkpoints().list(10).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
