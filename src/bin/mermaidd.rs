#[cfg(unix)]
use anyhow::{Context, Result};

#[cfg(unix)]
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

#[cfg(unix)]
const DEFAULT_TCP_ADDR: &str = "127.0.0.1:39871";

#[cfg(unix)]
#[tokio::main]
async fn main() -> Result<()> {
    let store = mermaid_cli::runtime::RuntimeStore::open_default()?;
    let socket_path = mermaid_cli::runtime::data_dir()?.join("mermaidd.sock");
    drop(store);

    if socket_path.exists() {
        std::fs::remove_file(&socket_path)
            .with_context(|| format!("failed to remove stale socket {}", socket_path.display()))?;
    }

    let listener = tokio::net::UnixListener::bind(&socket_path)
        .with_context(|| format!("failed to bind {}", socket_path.display()))?;
    println!("mermaidd listening on {}", socket_path.display());

    maybe_spawn_tcp_listener().await;

    loop {
        let (stream, _) = listener.accept().await?;
        tokio::spawn(async move {
            if let Err(err) = handle_stream(stream).await {
                tracing::warn!(error = %err, "mermaidd client failed");
            }
        });
    }
}

#[cfg(unix)]
async fn maybe_spawn_tcp_listener() {
    if std::env::var("MERMAID_DAEMON_DISABLE_TCP")
        .is_ok_and(|value| value == "1" || value == "true")
    {
        return;
    }
    let addr =
        std::env::var("MERMAID_DAEMON_TCP_ADDR").unwrap_or_else(|_| DEFAULT_TCP_ADDR.to_string());
    match tokio::net::TcpListener::bind(&addr).await {
        Ok(listener) => {
            if let Ok(local_addr) = listener.local_addr() {
                if let Ok(dir) = mermaid_cli::runtime::data_dir() {
                    let _ = std::fs::write(dir.join("mermaidd.tcp"), local_addr.to_string());
                }
                println!("mermaidd tcp listening on {}", local_addr);
            }
            tokio::spawn(async move {
                loop {
                    match listener.accept().await {
                        Ok((stream, _)) => {
                            tokio::spawn(async move {
                                if let Err(err) = handle_remote_stream(stream).await {
                                    tracing::warn!(error = %err, "mermaidd tcp client failed");
                                }
                            });
                        },
                        Err(err) => {
                            tracing::warn!(error = %err, "mermaidd tcp accept failed");
                            break;
                        },
                    }
                }
            });
        },
        Err(err) => {
            tracing::warn!(addr = %addr, error = %err, "mermaidd tcp listener disabled");
        },
    }
}

#[cfg(unix)]
async fn handle_stream<S>(stream: S) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    handle_stream_inner(stream, false).await
}

#[cfg(unix)]
async fn handle_remote_stream<S>(stream: S) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    handle_stream_inner(stream, true).await
}

#[cfg(unix)]
async fn handle_stream_inner<S>(stream: S, require_auth: bool) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    let response = handle_command(line.trim(), require_auth).await?;
    let mut stream = reader.into_inner();
    stream.write_all(response.to_string().as_bytes()).await?;
    stream.write_all(b"\n").await?;
    stream.shutdown().await?;
    Ok(())
}

#[cfg(unix)]
async fn handle_command(command: &str, require_auth: bool) -> Result<serde_json::Value> {
    if command.starts_with('{') {
        let body: serde_json::Value = serde_json::from_str(command)?;
        return handle_json_command(&body, require_auth).await;
    }

    if require_auth && !matches!(command, "" | "health") {
        return Ok(serde_json::json!({
            "ok": false,
            "error": "unauthorized: TCP control requires JSON auth.token or MERMAID_DAEMON_TOKEN",
        }));
    }

    let store = mermaid_cli::runtime::RuntimeStore::open_default()?;
    match command {
        "" | "health" => Ok(serde_json::json!({
            "ok": true,
            "service": "mermaidd",
            "database": store.path().display().to_string(),
        })),
        "tasks" => Ok(serde_json::json!({
            "ok": true,
            "tasks": store.tasks().list(50)?,
        })),
        "sessions" => Ok(serde_json::json!({
            "ok": true,
            "sessions": store.sessions().list(50)?,
        })),
        "processes" => Ok(serde_json::json!({
            "ok": true,
            "processes": store.processes().list(50)?,
        })),
        "approvals" => Ok(serde_json::json!({
            "ok": true,
            "approvals": store.approvals().list_pending()?,
        })),
        "tool_runs" => Ok(serde_json::json!({
            "ok": true,
            "tool_runs": store.tool_runs().list(100)?,
        })),
        "checkpoints" => Ok(serde_json::json!({
            "ok": true,
            "checkpoints": store.checkpoints().list(50)?,
        })),
        "memory" => Ok(serde_json::json!({
            "ok": true,
            "memory": store.memory().list(None, None)?,
        })),
        "plugins" => Ok(serde_json::json!({
            "ok": true,
            "plugins": store.plugins().list()?,
        })),
        "pairings" => Ok(serde_json::json!({
            "ok": true,
            "pairings": store.pairing_tokens().list()?,
        })),
        "snapshot" => Ok(serde_json::to_value(
            mermaid_cli::runtime::RuntimeService::from_store(store).snapshot()?,
        )?),
        other => Ok(serde_json::json!({
            "ok": false,
            "error": format!("unknown command: {}", other),
            "commands": ["health", "sessions", "tasks", "processes", "approvals", "tool_runs", "checkpoints", "memory", "plugins", "pairings", "snapshot"],
            "json_commands": ["create_task", "run", "update_task", "session_messages", "runtime_snapshot", "runtime_dashboard", "runtime_diagnostics", "runtime_hygiene_preview", "runtime_hygiene_archive", "runtime_task_detail", "runtime_approval_detail", "runtime_checkpoint_detail", "runtime_tasks", "runtime_processes", "runtime_approvals", "runtime_tool_runs", "runtime_checkpoints", "runtime_memory", "runtime_plugins", "logs", "stop_process", "restart_process", "open_process", "ports", "remember", "memory_edit", "forget", "restore_checkpoint", "approve", "deny", "plugin_preview", "plugin_install", "set_plugin_enabled", "model_info", "set_safety_mode", "pair"],
        })),
    }
}

#[cfg(unix)]
async fn handle_json_command(
    body: &serde_json::Value,
    require_remote_auth: bool,
) -> Result<serde_json::Value> {
    let command = body
        .get("command")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    if command == "health" {
        let store = mermaid_cli::runtime::RuntimeStore::open_default()?;
        return Ok(serde_json::json!({
            "ok": true,
            "service": "mermaidd",
            "database": store.path().display().to_string(),
        }));
    }
    if (require_remote_auth || command_requires_auth(command)) && !authorize(body)? {
        return Ok(serde_json::json!({
            "ok": false,
            "error": "unauthorized: set MERMAID_DAEMON_TOKEN or include auth.token",
        }));
    }
    match command {
        "create_task" => {
            let store = mermaid_cli::runtime::RuntimeStore::open_default()?;
            let title = str_field(body, "title")?;
            let project_path = str_field(body, "project_path")?;
            let model_id = str_field(body, "model_id")?;
            let task = store.tasks().create(mermaid_cli::runtime::NewTask::new(
                title,
                project_path,
                model_id,
            ))?;
            Ok(serde_json::json!({"ok": true, "task": task}))
        },
        "session_messages" => {
            let store = mermaid_cli::runtime::RuntimeStore::open_default()?;
            let id = str_field(body, "id")?;
            Ok(serde_json::json!({
                "ok": true,
                "session": store.sessions().get(id)?,
                "messages": store.messages().list_for_session(id)?,
            }))
        },
        "snapshot" | "runtime_snapshot" => {
            let service = mermaid_cli::runtime::RuntimeService::open_default()?;
            Ok(serde_json::to_value(service.snapshot()?)?)
        },
        "runtime_dashboard" => {
            let service = mermaid_cli::runtime::RuntimeService::open_default()?;
            Ok(serde_json::to_value(service.dashboard()?)?)
        },
        "runtime_diagnostics" => {
            let service = mermaid_cli::runtime::RuntimeService::open_default()?;
            Ok(serde_json::to_value(service.diagnostics()?)?)
        },
        "runtime_hygiene_preview" => {
            let service = mermaid_cli::runtime::RuntimeService::open_default()?;
            Ok(serde_json::to_value(service.hygiene_preview()?)?)
        },
        "runtime_hygiene_archive" => {
            let service = mermaid_cli::runtime::RuntimeService::open_default()?;
            Ok(serde_json::to_value(service.hygiene_archive()?)?)
        },
        "runtime_task_detail" => {
            let service = mermaid_cli::runtime::RuntimeService::open_default()?;
            Ok(serde_json::to_value(
                service.task_detail(str_field(body, "id")?)?,
            )?)
        },
        "runtime_approval_detail" => {
            let service = mermaid_cli::runtime::RuntimeService::open_default()?;
            Ok(serde_json::to_value(
                service.approval_detail(str_field(body, "id")?)?,
            )?)
        },
        "runtime_checkpoint_detail" => {
            let service = mermaid_cli::runtime::RuntimeService::open_default()?;
            Ok(serde_json::to_value(
                service.checkpoint_detail(str_field(body, "id")?)?,
            )?)
        },
        "runtime_tasks" => {
            let limit = body.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as usize;
            let service = mermaid_cli::runtime::RuntimeService::open_default()?;
            Ok(serde_json::json!({"ok": true, "items": service.list_tasks(limit)?}))
        },
        "runtime_processes" => {
            let limit = body.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as usize;
            let service = mermaid_cli::runtime::RuntimeService::open_default()?;
            Ok(serde_json::json!({"ok": true, "items": service.list_processes(limit)?}))
        },
        "runtime_approvals" => {
            let service = mermaid_cli::runtime::RuntimeService::open_default()?;
            Ok(serde_json::json!({"ok": true, "items": service.list_approvals()?}))
        },
        "runtime_tool_runs" => {
            let limit = body.get("limit").and_then(|v| v.as_u64()).unwrap_or(100) as usize;
            let service = mermaid_cli::runtime::RuntimeService::open_default()?;
            Ok(serde_json::json!({"ok": true, "items": service.list_tool_runs(limit)?}))
        },
        "runtime_checkpoints" => {
            let limit = body.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as usize;
            let service = mermaid_cli::runtime::RuntimeService::open_default()?;
            Ok(serde_json::json!({"ok": true, "items": service.list_checkpoints(limit)?}))
        },
        "runtime_memory" => {
            let service = mermaid_cli::runtime::RuntimeService::open_default()?;
            Ok(serde_json::json!({
                "ok": true,
                "items": service.list_memory(body.get("project_path").and_then(|v| v.as_str()))?,
            }))
        },
        "runtime_plugins" => {
            let service = mermaid_cli::runtime::RuntimeService::open_default()?;
            Ok(serde_json::json!({"ok": true, "items": service.list_plugins()?}))
        },
        "run" => {
            let prompt = str_field(body, "prompt")?.to_string();
            let project_path = match body.get("project_path").and_then(|v| v.as_str()) {
                Some(path) if !path.is_empty() => std::path::PathBuf::from(path),
                _ => std::env::current_dir()?,
            };
            let config = mermaid_cli::app::load_config().unwrap_or_default();
            let model_id = match body.get("model_id").and_then(|v| v.as_str()) {
                Some(model_id) if !model_id.is_empty() => model_id.to_string(),
                _ => mermaid_cli::app::resolve_model_id(None, &config).await?,
            };
            let store = mermaid_cli::runtime::RuntimeStore::open_default()?;
            let task = store.tasks().create(mermaid_cli::runtime::NewTask::new(
                task_title_from_prompt(&prompt),
                project_path.display().to_string(),
                model_id.clone(),
            ))?;
            store.tasks().update_status(
                &task.id,
                mermaid_cli::runtime::TaskStatus::Running,
                None,
            )?;
            let _ = mermaid_cli::runtime::run_plugin_hooks(
                "task_start",
                &serde_json::json!({
                    "id": task.id.clone(),
                    "title": task.title.clone(),
                    "project_path": task.project_path.clone(),
                    "model_id": task.model_id.clone(),
                }),
            );

            let task_id = task.id.clone();
            tokio::spawn(async move {
                let result = mermaid_cli::app::run_non_interactive_with(
                    config,
                    project_path,
                    model_id,
                    prompt,
                    mermaid_cli::app::RunOptions {
                        task_id: Some(task_id.clone()),
                        ..mermaid_cli::app::RunOptions::default()
                    },
                )
                .await;
                if let Ok(store) = mermaid_cli::runtime::RuntimeStore::open_default() {
                    match result {
                        Ok(result) if result.errors.is_empty() => {
                            let _ = store.tasks().update_status(
                                &task_id,
                                mermaid_cli::runtime::TaskStatus::Completed,
                                Some(&result.response),
                            );
                            let _ = mermaid_cli::runtime::run_plugin_hooks(
                                "task_stop",
                                &serde_json::json!({
                                    "id": task_id.clone(),
                                    "status": "completed",
                                    "final_report": result.response.clone(),
                                }),
                            );
                        },
                        Ok(result) => {
                            let _ = store.tasks().update_status(
                                &task_id,
                                mermaid_cli::runtime::TaskStatus::Failed,
                                Some(&result.errors.join("\n")),
                            );
                            let _ = mermaid_cli::runtime::run_plugin_hooks(
                                "task_stop",
                                &serde_json::json!({
                                    "id": task_id.clone(),
                                    "status": "failed",
                                    "final_report": result.errors.join("\n"),
                                }),
                            );
                        },
                        Err(err) => {
                            let _ = store.tasks().update_status(
                                &task_id,
                                mermaid_cli::runtime::TaskStatus::Failed,
                                Some(&err.to_string()),
                            );
                            let _ = mermaid_cli::runtime::run_plugin_hooks(
                                "task_stop",
                                &serde_json::json!({
                                    "id": task_id.clone(),
                                    "status": "failed",
                                    "final_report": err.to_string(),
                                }),
                            );
                        },
                    }
                }
            });

            Ok(serde_json::json!({"ok": true, "task": task}))
        },
        "update_task" => {
            let store = mermaid_cli::runtime::RuntimeStore::open_default()?;
            let id = str_field(body, "id")?;
            let status = match str_field(body, "status")? {
                "queued" => mermaid_cli::runtime::TaskStatus::Queued,
                "running" => mermaid_cli::runtime::TaskStatus::Running,
                "waiting_for_approval" => mermaid_cli::runtime::TaskStatus::WaitingForApproval,
                "blocked" => mermaid_cli::runtime::TaskStatus::Blocked,
                "completed" => mermaid_cli::runtime::TaskStatus::Completed,
                "failed" => mermaid_cli::runtime::TaskStatus::Failed,
                "cancelled" => mermaid_cli::runtime::TaskStatus::Cancelled,
                other => anyhow::bail!("unknown task status: {}", other),
            };
            let report = body.get("final_report").and_then(|v| v.as_str());
            store.tasks().update_status(id, status, report)?;
            Ok(serde_json::json!({"ok": true}))
        },
        "remember" => {
            let service = mermaid_cli::runtime::RuntimeService::open_default()?;
            let source = body
                .get("source")
                .and_then(|v| v.as_str())
                .unwrap_or("daemon");
            let entry = service.remember_memory(
                body.get("project_path")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                str_field(body, "key")?,
                str_field(body, "value")?,
                source,
            )?;
            Ok(serde_json::json!({"ok": true, "item": entry, "memory": entry}))
        },
        "memory_edit" => {
            let service = mermaid_cli::runtime::RuntimeService::open_default()?;
            let source = body
                .get("source")
                .and_then(|v| v.as_str())
                .unwrap_or("daemon");
            let entry =
                service.edit_memory(str_field(body, "id")?, str_field(body, "value")?, source)?;
            Ok(serde_json::json!({"ok": true, "item": entry, "memory": entry}))
        },
        "forget" => {
            let service = mermaid_cli::runtime::RuntimeService::open_default()?;
            service.forget_memory(str_field(body, "id")?)?;
            Ok(serde_json::json!({"ok": true}))
        },
        "logs" => {
            let service = mermaid_cli::runtime::RuntimeService::open_default()?;
            Ok(serde_json::to_value(service.process_log(
                str_field(body, "id")?,
                body.get("tail_bytes").and_then(|v| v.as_u64()),
            )?)?)
        },
        "stop_process" => {
            let service = mermaid_cli::runtime::RuntimeService::open_default()?;
            let process = service.stop_process(str_field(body, "id")?)?;
            Ok(serde_json::json!({"ok": true, "item": process, "process": process}))
        },
        "restart_process" => {
            let service = mermaid_cli::runtime::RuntimeService::open_default()?;
            let process = service.restart_process(str_field(body, "id")?)?;
            Ok(serde_json::json!({"ok": true, "item": process, "process": process}))
        },
        "open_process" => {
            let service = mermaid_cli::runtime::RuntimeService::open_default()?;
            Ok(serde_json::to_value(
                service.open_process(str_field(body, "id")?)?,
            )?)
        },
        "ports" => {
            let service = mermaid_cli::runtime::RuntimeService::open_default()?;
            Ok(serde_json::to_value(service.ports()?)?)
        },
        "restore_checkpoint" => {
            let manifest = mermaid_cli::runtime::restore_checkpoint(str_field(body, "id")?)?;
            Ok(serde_json::json!({"ok": true, "checkpoint": manifest}))
        },
        "approve" => {
            let id = str_field(body, "id")?;
            let result = mermaid_cli::runtime::approve_and_replay(id)?;
            Ok(
                serde_json::json!({"ok": true, "approval": result.approval, "replayed": result.replayed, "summary": result.summary}),
            )
        },
        "deny" => {
            let id = str_field(body, "id")?;
            let result = mermaid_cli::runtime::deny_approval(id)?;
            Ok(
                serde_json::json!({"ok": true, "approval": result.approval, "replayed": result.replayed, "summary": result.summary}),
            )
        },
        "plugin_preview" => {
            let preview = mermaid_cli::runtime::plugin_permission_preview(std::path::Path::new(
                str_field(body, "path")?,
            ))?;
            Ok(serde_json::json!({"ok": true, "preview": preview}))
        },
        "plugin_install" => {
            let path = std::path::Path::new(str_field(body, "path")?);
            let preview = mermaid_cli::runtime::plugin_permission_preview(path)?;
            let plugin = mermaid_cli::runtime::install_plugin_from_path(path)?;
            Ok(serde_json::json!({"ok": true, "preview": preview, "plugin": plugin}))
        },
        "set_plugin_enabled" => {
            let service = mermaid_cli::runtime::RuntimeService::open_default()?;
            service.set_plugin_enabled(str_field(body, "id")?, bool_field(body, "enabled")?)?;
            Ok(serde_json::json!({"ok": true}))
        },
        "set_safety_mode" => {
            let service = mermaid_cli::runtime::RuntimeService::open_default()?;
            let safety = service.set_safety_mode(str_field(body, "mode")?)?;
            Ok(serde_json::json!({"ok": true, "safety": safety}))
        },
        "model_info" => {
            let service = mermaid_cli::runtime::RuntimeService::open_default()?;
            Ok(
                serde_json::json!({"ok": true, "model": service.model_info(str_field(body, "model")?)}),
            )
        },
        "pair" => {
            let store = mermaid_cli::runtime::RuntimeStore::open_default()?;
            let label = body.get("label").and_then(|v| v.as_str());
            let (token, hash) = match body.get("token_hash").and_then(|v| v.as_str()) {
                Some(hash) if !hash.is_empty() => (None, hash.to_string()),
                _ => {
                    let (token, hash) = mermaid_cli::runtime::generate_pairing_token()?;
                    (Some(token), hash)
                },
            };
            let record = store.pairing_tokens().create(&hash, label)?;
            Ok(serde_json::json!({"ok": true, "pairing": record, "token": token}))
        },
        other => Ok(serde_json::json!({
            "ok": false,
            "error": format!("unknown command: {}", other),
        })),
    }
}

#[cfg(unix)]
fn command_requires_auth(command: &str) -> bool {
    matches!(
        command,
        "create_task"
            | "run"
            | "update_task"
            | "remember"
            | "memory_edit"
            | "forget"
            | "restore_checkpoint"
            | "approve"
            | "deny"
            | "stop_process"
            | "restart_process"
            | "open_process"
            | "plugin_install"
            | "set_plugin_enabled"
            | "set_safety_mode"
            | "runtime_hygiene_archive"
            | "pair"
    )
}

#[cfg(unix)]
fn authorize(body: &serde_json::Value) -> Result<bool> {
    let Some(token) = body
        .get("auth")
        .and_then(|v| v.get("token"))
        .and_then(|v| v.as_str())
        .or_else(|| body.get("token").and_then(|v| v.as_str()))
    else {
        return Ok(false);
    };
    let hash = mermaid_cli::runtime::hash_pairing_token(token);
    let store = mermaid_cli::runtime::RuntimeStore::open_default()?;
    let Some(record) = store.pairing_tokens().get_enabled_by_hash(&hash)? else {
        return Ok(false);
    };
    store.pairing_tokens().mark_used(&record.id)?;
    Ok(true)
}

#[cfg(unix)]
fn task_title_from_prompt(prompt: &str) -> String {
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

#[cfg(unix)]
fn str_field<'a>(body: &'a serde_json::Value, name: &str) -> Result<&'a str> {
    body.get(name)
        .and_then(|v| v.as_str())
        .filter(|v| !v.is_empty())
        .with_context(|| format!("missing string field `{}`", name))
}

#[cfg(unix)]
fn bool_field(body: &serde_json::Value, name: &str) -> Result<bool> {
    body.get(name)
        .and_then(|v| v.as_bool())
        .with_context(|| format!("missing bool field `{}`", name))
}

#[cfg(all(test, unix))]
mod tests {
    fn temp_db(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("mermaidd_runtime_hygiene_{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir.join("runtime.sqlite3")
    }

    #[test]
    fn mutating_json_commands_require_auth_on_local_socket() {
        for command in [
            "create_task",
            "run",
            "update_task",
            "remember",
            "memory_edit",
            "forget",
            "restore_checkpoint",
            "approve",
            "deny",
            "stop_process",
            "restart_process",
            "open_process",
            "plugin_install",
            "set_plugin_enabled",
            "set_safety_mode",
            "runtime_hygiene_archive",
            "pair",
        ] {
            assert!(super::command_requires_auth(command), "{command}");
        }
        for command in [
            "snapshot",
            "runtime_snapshot",
            "runtime_dashboard",
            "runtime_diagnostics",
            "runtime_hygiene_preview",
            "runtime_task_detail",
            "runtime_approval_detail",
            "runtime_checkpoint_detail",
            "runtime_tasks",
            "runtime_processes",
            "runtime_approvals",
            "runtime_tool_runs",
            "runtime_checkpoints",
            "runtime_memory",
            "runtime_plugins",
            "logs",
            "ports",
        ] {
            assert!(!super::command_requires_auth(command), "{command}");
        }
    }

    #[test]
    fn runtime_hygiene_preview_matches_test_artifacts_and_archive_is_idempotent() {
        let path = temp_db("preview");
        let store = mermaid_cli::runtime::RuntimeStore::open(&path).expect("open store");
        let checkpoint = store
            .checkpoints()
            .create(mermaid_cli::runtime::NewCheckpoint {
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
            .create(mermaid_cli::runtime::NewApproval {
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

        let service = mermaid_cli::runtime::RuntimeService::from_store(store);
        let preview = service.hygiene_preview().expect("preview");
        assert_eq!(preview.counts.approvals, 1);
        assert_eq!(preview.counts.checkpoints, 1);
        let archived = service.hygiene_archive().expect("archive");
        assert_eq!(archived.archived.total, 2);
        let archived_again = service.hygiene_archive().expect("archive again");
        assert_eq!(archived_again.archived.total, 0);
        let store = mermaid_cli::runtime::RuntimeStore::open(&path).expect("reopen store");
        assert!(store.approvals().list_pending().unwrap().is_empty());
        assert!(store.checkpoints().list(10).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}

#[cfg(not(unix))]
fn main() {
    eprintln!("mermaidd currently supports Unix sockets only");
    std::process::exit(1);
}
