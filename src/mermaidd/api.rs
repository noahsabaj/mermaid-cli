//! The JSON command API: tasks, runtime reads, processes, admin.

use anyhow::Result;

use super::scheduler::{scheduler, task_title_from_prompt};

pub(super) async fn handle_command(command: &str, require_auth: bool) -> Result<serde_json::Value> {
    if command.starts_with('{') {
        let body: serde_json::Value = serde_json::from_str(command)?;
        return handle_json_command(&body, require_auth).await;
    }

    // TCP control requires JSON-with-token for EVERY command including health,
    // so a bare connection can't even fingerprint the daemon / DB path.
    // Plaintext commands are honored only on the (0600) local socket.
    if require_auth {
        return Ok(serde_json::json!({
            "ok": false,
            "error": "unauthorized: TCP control requires JSON auth.token or MERMAID_DAEMON_TOKEN",
        }));
    }

    // Only `health` (liveness + DB path, no sensitive rows) is served in the
    // plaintext form. The plaintext DATA commands used to serve tasks, sessions,
    // snapshots, etc. with NO auth — bypassing the #21 pairing-token gate that
    // the JSON `runtime_*` reads enforce, so any same-UID process could read
    // session messages and full DB snapshots straight off the socket. Nothing in
    // the repo speaks plaintext (every client sends JSON, including `health`), so
    // these are removed outright rather than gated. Everything sensitive now goes
    // through the token-checked JSON path only.
    match command {
        "" | "health" => {
            let store = mermaid_runtime::RuntimeStore::open_default()?;
            Ok(serde_json::json!({
                "ok": true,
                "service": "mermaidd",
                "database": store.path().display().to_string(),
            }))
        },
        other => Ok(serde_json::json!({
            "ok": false,
            "error": format!("unknown command: {}", other),
            "commands": ["health"],
            "json_commands": ["create_task", "run", "cancel_task", "send_to_task", "update_task", "session_messages", "snapshot", "runtime_snapshot", "runtime_dashboard", "runtime_diagnostics", "runtime_hygiene_preview", "runtime_hygiene_archive", "runtime_task_detail", "runtime_approval_detail", "runtime_checkpoint_detail", "runtime_tasks", "runtime_processes", "runtime_approvals", "runtime_tool_runs", "runtime_checkpoints", "runtime_plugins", "logs", "stop_process", "restart_process", "open_process", "ports", "restore_checkpoint", "approve", "deny", "plugin_preview", "plugin_install", "set_plugin_enabled", "model_info", "set_safety_mode", "pair", "subscribe_task"],
        })),
    }
}

pub(super) async fn handle_json_command(
    body: &serde_json::Value,
    require_remote_auth: bool,
) -> Result<serde_json::Value> {
    use crate::runtime_client::DaemonRequest;
    // Parse FIRST so auth gating comes from the exhaustive typed matrix; a
    // malformed request answers with a serde error naming the problem
    // instead of a silent `unknown command`.
    let request: DaemonRequest = match serde_json::from_value(body.clone()) {
        Ok(request) => request,
        Err(err) => {
            return Ok(serde_json::json!({
                "ok": false,
                "error": format!("invalid request: {err}"),
            }));
        },
    };
    if (require_remote_auth || request.requires_auth()) && !authorize(body)? {
        return Ok(serde_json::json!({
            "ok": false,
            "error": "unauthorized: set MERMAID_DAEMON_TOKEN or include auth.token",
        }));
    }
    match request {
        DaemonRequest::Health => {
            let store = mermaid_runtime::RuntimeStore::open_default()?;
            Ok(serde_json::json!({
                "ok": true,
                "service": "mermaidd",
                "database": store.path().display().to_string(),
            }))
        },
        req @ (DaemonRequest::CreateTask { .. }
        | DaemonRequest::Run { .. }
        | DaemonRequest::CancelTask { .. }
        | DaemonRequest::SendToTask { .. }
        | DaemonRequest::UpdateTask { .. }) => handle_task_command(req).await,
        req @ (DaemonRequest::SessionMessages { .. }
        | DaemonRequest::Snapshot
        | DaemonRequest::RuntimeDashboard
        | DaemonRequest::RuntimeDiagnostics
        | DaemonRequest::RuntimeHygienePreview
        | DaemonRequest::RuntimeHygieneArchive
        | DaemonRequest::RuntimeTaskDetail { .. }
        | DaemonRequest::RuntimeApprovalDetail { .. }
        | DaemonRequest::RuntimeCheckpointDetail { .. }
        | DaemonRequest::RuntimeTasks { .. }
        | DaemonRequest::RuntimeProcesses { .. }
        | DaemonRequest::RuntimeToolRuns { .. }
        | DaemonRequest::RuntimeCheckpoints { .. }
        | DaemonRequest::RuntimeApprovals
        | DaemonRequest::RuntimePlugins
        | DaemonRequest::ModelInfo { .. }) => handle_runtime_read(req),
        req @ (DaemonRequest::Logs { .. }
        | DaemonRequest::StopProcess { .. }
        | DaemonRequest::RestartProcess { .. }
        | DaemonRequest::OpenProcess { .. }
        | DaemonRequest::Ports) => handle_process_command(req),
        req @ (DaemonRequest::RestoreCheckpoint { .. }
        | DaemonRequest::Approve { .. }
        | DaemonRequest::Deny { .. }
        | DaemonRequest::PluginPreview { .. }
        | DaemonRequest::PluginInstall { .. }
        | DaemonRequest::SetPluginEnabled { .. }
        | DaemonRequest::SetSafetyMode { .. }
        | DaemonRequest::Pair { .. }) => handle_admin_command(req),
        DaemonRequest::SubscribeTask { .. } => Ok(serde_json::json!({
            // Streaming requests are classified in handle_stream_inner and
            // never reach the one-shot dispatcher; answer defensively.
            "ok": false,
            "error": "subscribe_task is a streaming command; it is handled at the connection layer",
        })),
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "one arm per task-family DaemonRequest variant; splitting would scatter the daemon's \
     dispatch table across helpers that each take the same store handle"
)]
pub(super) async fn handle_task_command(
    request: crate::runtime_client::DaemonRequest,
) -> Result<serde_json::Value> {
    use crate::runtime_client::DaemonRequest;
    match request {
        DaemonRequest::CreateTask {
            title,
            project_path,
            model_id,
        } => {
            let store = mermaid_runtime::RuntimeStore::open_default()?;
            // F18 (RC-E): tag daemon-created tasks so the startup reconcile may
            // recover them — interactive CLI tasks stay un-owned and are spared.
            let task = store.tasks().create(
                mermaid_runtime::NewTask::new(title, project_path, model_id).daemon_owned(),
            )?;
            Ok(serde_json::json!({"ok": true, "task": task}))
        },
        DaemonRequest::Run {
            prompt,
            project_path,
            model_id,
            priority,
        } => {
            // Empty string means unset on every optional field — exact wire
            // behavior of the stringly dispatch this replaced.
            let project_path = match project_path.as_deref() {
                Some(path) if !path.is_empty() => std::path::PathBuf::from(path),
                _ => std::env::current_dir()?,
            };
            let config = crate::app::load_config().unwrap_or_default();
            let model_id = match model_id.as_deref() {
                Some(model_id) if !model_id.is_empty() => model_id.to_string(),
                _ => crate::app::resolve_model_id(None, &config).await?,
            };
            let priority = match priority.as_deref() {
                None | Some("") | Some("normal") => mermaid_runtime::TaskPriority::Normal,
                Some("high") => mermaid_runtime::TaskPriority::High,
                Some("low") => mermaid_runtime::TaskPriority::Low,
                // Respond, don't bail: a bail here propagates out of the
                // handler and the client sees a silent connection drop instead
                // of an error.
                Some(other) => {
                    return Ok(serde_json::json!({
                        "ok": false,
                        "error": format!("unknown priority: {other} (expected low|normal|high)"),
                    }));
                },
            };
            let store = mermaid_runtime::RuntimeStore::open_default()?;
            // Enqueue only — the scheduler claims it when a permit frees, so a
            // burst of runs executes bounded by `daemon.max_concurrent_tasks`
            // instead of stampeding the GPU. The full prompt is persisted for
            // that deferred execution (and survives a daemon restart).
            // F18 (RC-E): daemon-owned, so a crash leaving it `Running` is
            // recovered by the next startup reconcile.
            let task = store.tasks().create(
                mermaid_runtime::NewTask::new(
                    task_title_from_prompt(&prompt),
                    project_path.display().to_string(),
                    model_id,
                )
                .daemon_owned()
                .with_prompt(prompt)
                .with_priority(priority),
            )?;
            scheduler().wake.notify_one();
            Ok(serde_json::json!({"ok": true, "task": task}))
        },
        DaemonRequest::SendToTask { id, text } => {
            let Some(mailbox) = scheduler().mailbox_for(&id) else {
                anyhow::bail!(
                    "task {id} is not running -- there is no reducer to send to.                      Queue a follow-up with `mermaid run --resume` once it finishes."
                )
            };
            // `try_send`, not `send`: the daemon answers requests on this
            // thread, and a run that is behind on its own effects must not
            // block every other client.
            mailbox
                .try_send(mermaid_domain::Msg::SubmitPrompt {
                    text,
                    attachment_ids: vec![],
                })
                .map_err(|gone| anyhow::anyhow!("task {id}: {gone}"))?;
            Ok(serde_json::json!({"ok": true, "id": id, "delivered": true}))
        },
        DaemonRequest::CancelTask { id } => {
            // Running here? Fire its token — the run injects `Msg::CancelTurn`
            // (the same graceful teardown as Esc in the TUI) and the executor
            // persists `cancelled` when it unwinds.
            let token = scheduler()
                .running
                .lock()
                .expect("scheduler running map poisoned")
                .get(&id)
                .cloned();
            if let Some(token) = token {
                token.cancel();
                return Ok(serde_json::json!({"ok": true, "id": id, "cancelling": true}));
            }
            // Not in-flight: a queued task is cancelled by flipping the row —
            // the claim query only ever picks `queued` rows, so this is safe.
            let store = mermaid_runtime::RuntimeStore::open_default()?;
            match store.tasks().get(&id)? {
                Some(task) if task.status == mermaid_runtime::TaskStatus::Queued => {
                    store.tasks().update_status(
                        &id,
                        mermaid_runtime::TaskStatus::Cancelled,
                        Some("cancelled before start"),
                    )?;
                    Ok(serde_json::json!({"ok": true, "task": store.tasks().get(&id)?}))
                },
                Some(task) => Ok(serde_json::json!({
                    "ok": false,
                    "error": format!("task {} is {} — not cancellable", id, task.status),
                })),
                None => Ok(serde_json::json!({
                    "ok": false,
                    "error": format!("task not found: {}", id),
                })),
            }
        },
        DaemonRequest::UpdateTask {
            id,
            status,
            final_report,
        } => {
            let store = mermaid_runtime::RuntimeStore::open_default()?;
            let status = match status.as_str() {
                "queued" => mermaid_runtime::TaskStatus::Queued,
                "running" => mermaid_runtime::TaskStatus::Running,
                "waiting_for_approval" => mermaid_runtime::TaskStatus::WaitingForApproval,
                "blocked" => mermaid_runtime::TaskStatus::Blocked,
                "completed" => mermaid_runtime::TaskStatus::Completed,
                "failed" => mermaid_runtime::TaskStatus::Failed,
                "cancelled" => mermaid_runtime::TaskStatus::Cancelled,
                other => anyhow::bail!("unknown task status: {other}"),
            };
            store
                .tasks()
                .update_status(&id, status, final_report.as_deref())?;
            Ok(serde_json::json!({"ok": true}))
        },
        other => anyhow::bail!("mis-routed task command: {other:?}"),
    }
}

pub(super) fn handle_runtime_read(
    request: crate::runtime_client::DaemonRequest,
) -> Result<serde_json::Value> {
    use crate::runtime_client::DaemonRequest;
    let service = crate::runtime_client::RuntimeService::open_default()?;
    match request {
        DaemonRequest::SessionMessages { id } => {
            // Served from the conversation files (see `transcript_rows`);
            // the `messages` table this used to read has never had a
            // writer, so it always answered with an empty list.
            let (session, messages) = service.session_messages(&id)?;
            Ok(serde_json::json!({
                "ok": true,
                "session": session,
                "messages": messages,
            }))
        },
        DaemonRequest::Snapshot => Ok(serde_json::to_value(service.snapshot()?)?),
        DaemonRequest::RuntimeDashboard => Ok(serde_json::to_value(service.dashboard()?)?),
        DaemonRequest::RuntimeDiagnostics => Ok(serde_json::to_value(service.diagnostics()?)?),
        DaemonRequest::RuntimeHygienePreview => {
            Ok(serde_json::to_value(service.hygiene_preview()?)?)
        },
        DaemonRequest::RuntimeHygieneArchive => {
            Ok(serde_json::to_value(service.hygiene_archive()?)?)
        },
        DaemonRequest::RuntimeTaskDetail { id } => {
            Ok(serde_json::to_value(service.task_detail(&id)?)?)
        },
        DaemonRequest::RuntimeApprovalDetail { id } => {
            Ok(serde_json::to_value(service.approval_detail(&id)?)?)
        },
        DaemonRequest::RuntimeCheckpointDetail { id } => {
            Ok(serde_json::to_value(service.checkpoint_detail(&id)?)?)
        },
        DaemonRequest::RuntimeTasks { limit } => {
            let limit = limit.unwrap_or(50) as usize;
            Ok(serde_json::json!({"ok": true, "items": service.list_tasks(limit)?}))
        },
        DaemonRequest::RuntimeProcesses { limit } => {
            let limit = limit.unwrap_or(50) as usize;
            Ok(serde_json::json!({"ok": true, "items": service.list_processes(limit)?}))
        },
        DaemonRequest::RuntimeToolRuns { limit } => {
            let limit = limit.unwrap_or(100) as usize;
            Ok(serde_json::json!({"ok": true, "items": service.list_tool_runs(limit)?}))
        },
        DaemonRequest::RuntimeCheckpoints { limit } => {
            let limit = limit.unwrap_or(50) as usize;
            Ok(serde_json::json!({"ok": true, "items": service.list_checkpoints(limit)?}))
        },
        DaemonRequest::RuntimeApprovals => {
            Ok(serde_json::json!({"ok": true, "items": service.list_approvals()?}))
        },
        DaemonRequest::RuntimePlugins => {
            Ok(serde_json::json!({"ok": true, "items": service.list_plugins()?}))
        },
        DaemonRequest::ModelInfo { model } => {
            Ok(serde_json::json!({"ok": true, "model": service.model_info(&model)}))
        },
        other => anyhow::bail!("mis-routed runtime read: {other:?}"),
    }
}

pub(super) fn handle_process_command(
    request: crate::runtime_client::DaemonRequest,
) -> Result<serde_json::Value> {
    use crate::runtime_client::DaemonRequest;
    let service = crate::runtime_client::RuntimeService::open_default()?;
    match request {
        DaemonRequest::Logs { id, tail_bytes } => {
            Ok(serde_json::to_value(service.process_log(&id, tail_bytes)?)?)
        },
        DaemonRequest::StopProcess { id } => {
            let process = service.stop_process(&id)?;
            Ok(serde_json::json!({"ok": true, "item": process, "process": process}))
        },
        DaemonRequest::RestartProcess { id } => {
            let process = service.restart_process(&id)?;
            Ok(serde_json::json!({"ok": true, "item": process, "process": process}))
        },
        DaemonRequest::OpenProcess { id } => Ok(serde_json::to_value(service.open_process(&id)?)?),
        DaemonRequest::Ports => Ok(serde_json::to_value(service.ports()?)?),
        other => anyhow::bail!("mis-routed process command: {other:?}"),
    }
}

pub(super) fn handle_admin_command(
    request: crate::runtime_client::DaemonRequest,
) -> Result<serde_json::Value> {
    use crate::runtime_client::DaemonRequest;
    match request {
        DaemonRequest::RestoreCheckpoint { id } => {
            let manifest = mermaid_runtime::restore_checkpoint(&id)?;
            Ok(serde_json::json!({"ok": true, "checkpoint": manifest}))
        },
        DaemonRequest::Approve { id } => {
            let result = mermaid_runtime::approve_and_replay(&id)?;
            Ok(
                serde_json::json!({"ok": true, "approval": result.approval, "replayed": result.replayed, "summary": result.summary}),
            )
        },
        DaemonRequest::Deny { id } => {
            let result = mermaid_runtime::deny_approval(&id)?;
            Ok(
                serde_json::json!({"ok": true, "approval": result.approval, "replayed": result.replayed, "summary": result.summary}),
            )
        },
        DaemonRequest::PluginPreview { path } => {
            let preview = mermaid_runtime::plugin_capability_preview(std::path::Path::new(&path))?;
            Ok(serde_json::json!({"ok": true, "preview": preview}))
        },
        DaemonRequest::PluginInstall { path } => {
            let path = std::path::Path::new(&path);
            let preview = mermaid_runtime::plugin_capability_preview(path)?;
            let plugin = mermaid_runtime::install_plugin_from_path(path)?;
            Ok(serde_json::json!({"ok": true, "preview": preview, "plugin": plugin}))
        },
        DaemonRequest::SetPluginEnabled { id, enabled } => {
            let service = crate::runtime_client::RuntimeService::open_default()?;
            service.set_plugin_enabled(&id, enabled)?;
            Ok(serde_json::json!({"ok": true}))
        },
        DaemonRequest::SetSafetyMode { mode } => {
            let service = crate::runtime_client::RuntimeService::open_default()?;
            let safety = service.set_safety_mode(&mode)?;
            Ok(serde_json::json!({"ok": true, "safety": safety}))
        },
        DaemonRequest::Pair {
            label,
            ttl_days,
            token_hash,
        } => {
            let store = mermaid_runtime::RuntimeStore::open_default()?;
            let ttl_days = ttl_days.unwrap_or(mermaid_runtime::DEFAULT_PAIRING_TTL_DAYS);
            // #65: clamp so a socket caller can't mint a never-expiring token by
            // sending ttl_days <= 0.
            let ttl_days = mermaid_runtime::clamp_pairing_ttl_days(ttl_days);
            let expires_at = mermaid_runtime::pairing_expiry_from_now(ttl_days);
            let (token, hash) = match token_hash.as_deref() {
                Some(hash) if !hash.is_empty() => (None, hash.to_string()),
                _ => {
                    let (token, hash) = mermaid_runtime::generate_pairing_token()?;
                    (Some(token), hash)
                },
            };
            let record =
                store
                    .pairing_tokens()
                    .create(&hash, label.as_deref(), expires_at.as_deref())?;
            Ok(serde_json::json!({"ok": true, "pairing": record, "token": token}))
        },
        other => anyhow::bail!("mis-routed admin command: {other:?}"),
    }
}

pub(super) fn authorize(body: &serde_json::Value) -> Result<bool> {
    let Some(token) = body
        .get("auth")
        .and_then(|v| v.get("token"))
        .and_then(|v| v.as_str())
        .or_else(|| body.get("token").and_then(|v| v.as_str()))
    else {
        return Ok(false);
    };
    let hash = mermaid_runtime::hash_pairing_token(token);
    let store = mermaid_runtime::RuntimeStore::open_default()?;
    // Constant-time hash match + expiry enforced inside verify_token.
    let Some(record) = store.pairing_tokens().verify_token(&hash)? else {
        return Ok(false);
    };
    store.pairing_tokens().mark_used(&record.id)?;
    Ok(true)
}
