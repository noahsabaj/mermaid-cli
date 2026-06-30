#[cfg(unix)]
use anyhow::{Context, Result};

#[cfg(unix)]
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

#[cfg(unix)]
const DEFAULT_TCP_ADDR: &str = "127.0.0.1:39871";

#[cfg(unix)]
const HELP: &str = "\
mermaidd — Mermaid's background daemon (durable runtime state, remote attach,
long-running process ownership).

Usage: mermaidd [--version | --help]

With no arguments it runs the daemon in the foreground, serving the control
socket. It is normally managed via the `mermaid daemon` subcommands (install,
start, stop, restart, status, logs) rather than invoked directly.
";

#[cfg(unix)]
#[derive(Debug, PartialEq, Eq)]
enum CliAction {
    Run,
    Version,
    Help,
    Unknown(String),
}

/// Classify `mermaidd`'s invocation. The daemon takes no real arguments, but it
/// is on `PATH` (and in the distro packages), so it must answer
/// `--version`/`--help` and reject unknown arguments rather than silently
/// starting the daemon: a `mermaidd --version` probe would otherwise boot a
/// foreground daemon and — because startup replaces the control socket — could
/// knock a running daemon off its socket.
#[cfg(unix)]
fn classify_args<I: IntoIterator<Item = String>>(args: I) -> CliAction {
    match args.into_iter().next().as_deref() {
        None => CliAction::Run,
        Some("--version" | "-V" | "version") => CliAction::Version,
        Some("--help" | "-h" | "help") => CliAction::Help,
        Some(other) => CliAction::Unknown(other.to_string()),
    }
}

/// Retention window for the startup GC (#130): archived approvals/checkpoints,
/// the events of long-finished tasks, and on-disk checkpoint dirs older than this
/// are pruned. Generous so nothing recently useful is dropped.
const RUNTIME_RETENTION_DAYS: i64 = 30;

#[cfg(unix)]
#[tokio::main]
async fn main() -> Result<()> {
    match classify_args(std::env::args().skip(1)) {
        CliAction::Run => {},
        CliAction::Version => {
            println!("mermaidd {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        },
        CliAction::Help => {
            print!("{HELP}");
            return Ok(());
        },
        CliAction::Unknown(arg) => {
            eprintln!("mermaidd: unrecognized argument '{arg}'\n\n{HELP}");
            std::process::exit(2);
        },
    }

    let store = mermaid_cli::runtime::RuntimeStore::open_default()?;
    let data_dir = mermaid_cli::runtime::data_dir()?;
    let socket_path = data_dir.join("mermaidd.sock");
    drop(store);

    // #66: the 0700 data dir is what makes the 0600 socket meaningful.
    // `open_default` warns but stays non-fatal on a chmod failure (a shared
    // CLI/test path); here, at the daemon's privilege boundary, refuse to serve
    // on a loose dir.
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&data_dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to lock data dir {} to 0700", data_dir.display()))?;
    }

    // Singleton guard (#131): hold an advisory flock for the daemon's whole
    // lifetime so two concurrent starts can't race the connect-probe → unlink →
    // bind dance below (one would unlink the other's fresh socket). flock
    // auto-releases on process exit/crash, so a dead daemon never wedges it.
    let lock_path = data_dir.join("mermaidd.lock");
    let _daemon_lock = match mermaid_cli::runtime::try_exclusive_lock(&lock_path)
        .with_context(|| format!("failed to open daemon lock {}", lock_path.display()))?
    {
        Some(file) => file,
        None => anyhow::bail!(
            "another mermaidd is starting or running (lock held on {}) — use `mermaid daemon restart`",
            lock_path.display()
        ),
    };

    // Only the lock holder reaches here, so this runs once per live daemon:
    // recover state a previous daemon left stranded by crashing (#120, #118) and
    // prune old runtime rows + checkpoint dirs (#130). All best-effort.
    if let Ok(store) = mermaid_cli::runtime::RuntimeStore::open_default() {
        match store.reconcile_after_restart() {
            Ok((tasks, claims)) if tasks + claims > 0 => {
                tracing::info!(
                    tasks,
                    claims,
                    "reconciled state stranded by a previous daemon"
                );
            },
            Ok(_) => {},
            Err(error) => tracing::warn!(error = %error, "startup reconcile failed"),
        }
        match store.gc(RUNTIME_RETENTION_DAYS) {
            Ok(removed) if removed > 0 => tracing::info!(removed, "gc pruned old runtime rows"),
            Ok(_) => {},
            Err(error) => tracing::warn!(error = %error, "startup gc failed"),
        }
    }
    if let Ok(removed) = mermaid_cli::runtime::gc_old_checkpoint_dirs(RUNTIME_RETENTION_DAYS)
        && removed > 0
    {
        tracing::info!(removed, "gc removed old checkpoint directories");
    }

    if socket_path.exists() {
        // Don't clobber a daemon that's already serving here. If something
        // accepts a connection on the socket, refuse — unlinking it would knock
        // the live daemon off its socket path (a `mermaidd --version`-style
        // probe or a second manual start). Only a stale socket left by a
        // crashed daemon — where connecting fails — is removed so we can rebind.
        if tokio::net::UnixStream::connect(&socket_path).await.is_ok() {
            anyhow::bail!(
                "a mermaidd daemon is already running on {} — use `mermaid daemon restart` to replace it",
                socket_path.display()
            );
        }
        std::fs::remove_file(&socket_path)
            .with_context(|| format!("failed to remove stale socket {}", socket_path.display()))?;
    }

    let listener = tokio::net::UnixListener::bind(&socket_path)
        .with_context(|| format!("failed to bind {}", socket_path.display()))?;
    // Restrict the control socket to the owning user (0600). Combined with the
    // 0700 data dir, no other local UID can reach the agent control plane. A
    // failed lockdown is fatal — never serve the control plane world-reachable.
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| {
                format!(
                    "failed to lock control socket {} to 0600",
                    socket_path.display()
                )
            })?;
    }
    println!("mermaidd listening on {}", socket_path.display());

    maybe_spawn_tcp_listener().await;

    // The socket lives in the 0700 data dir we own, so its file-owner uid is our
    // uid; reject any peer whose uid doesn't match (#66) — defense-in-depth
    // behind the 0600 perms, via std `MetadataExt::uid` (no extra crate).
    use std::os::unix::fs::MetadataExt;
    let owner_uid = std::fs::metadata(&socket_path)
        .with_context(|| format!("failed to stat control socket {}", socket_path.display()))?
        .uid();

    loop {
        let (stream, _) = listener.accept().await?;
        match stream.peer_cred() {
            Ok(cred) if uid_allowed(cred.uid(), owner_uid) => {},
            Ok(cred) => {
                tracing::warn!(
                    peer_uid = cred.uid(),
                    owner_uid,
                    "rejecting unix client: uid mismatch"
                );
                continue;
            },
            Err(err) => {
                tracing::warn!(error = %err, "rejecting unix client: peer_cred failed");
                continue;
            },
        }
        tokio::spawn(async move {
            if let Err(err) = handle_stream(stream).await {
                tracing::warn!(error = %err, "mermaidd client failed");
            }
        });
    }
}

/// Whether a connecting peer's uid may drive the control plane: the socket owner
/// (us) or root (already omnipotent locally, so rejecting it adds no security and
/// breaks admin tooling).
#[cfg(unix)]
fn uid_allowed(peer_uid: u32, owner_uid: u32) -> bool {
    peer_uid == owner_uid || peer_uid == 0
}

#[cfg(unix)]
async fn maybe_spawn_tcp_listener() {
    // TCP control is OFF by default — it exposes the agent control plane to
    // every local UID and anything that can reach loopback. Opt in with
    // MERMAID_DAEMON_ENABLE_TCP=1. Unlike the Unix socket, a TcpStream carries no
    // peer credentials (#66), so mandatory token auth is its only gate.
    if !std::env::var("MERMAID_DAEMON_ENABLE_TCP")
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
                    let tcp_file = dir.join("mermaidd.tcp");
                    if std::fs::write(&tcp_file, local_addr.to_string()).is_ok() {
                        use std::os::unix::fs::PermissionsExt;
                        // The hint file holds a loopback address, not a secret, so
                        // a chmod failure warns rather than killing the listener.
                        if let Err(err) = std::fs::set_permissions(
                            &tcp_file,
                            std::fs::Permissions::from_mode(0o600),
                        ) {
                            tracing::warn!(file = %tcp_file.display(), error = %err, "failed to lock tcp hint file to 0600");
                        }
                    }
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
    // Bounded read: a pre-auth client (especially over TCP) must not be able to
    // stream bytes without a newline and grow this buffer without bound (#22).
    let mut reader = BufReader::new(stream);
    let line = match mermaid_cli::utils::read_line_capped(
        &mut reader,
        mermaid_cli::constants::MAX_DAEMON_COMMAND_BYTES,
    )
    .await?
    {
        mermaid_cli::utils::CappedLine::Line(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        mermaid_cli::utils::CappedLine::TooLong => {
            anyhow::bail!("daemon command exceeded size cap")
        },
        mermaid_cli::utils::CappedLine::Eof => String::new(),
    };
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

    // TCP control requires JSON-with-token for EVERY command including health,
    // so a bare connection can't even fingerprint the daemon / DB path.
    // Plaintext commands are honored only on the (0600) local socket.
    if require_auth {
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
        "plugins" => Ok(serde_json::json!({
            "ok": true,
            "plugins": store.plugins().list()?,
        })),
        // NOTE: there is deliberately no `pairings` command. Listing token
        // hashes + labels over the socket exposed them to any same-UID process;
        // pairing-token inspection now goes through the owner-only `mermaid pair
        // list` CLI, which reads the store directly.
        "snapshot" => Ok(serde_json::to_value(
            mermaid_cli::runtime::RuntimeService::from_store(store).snapshot()?,
        )?),
        other => Ok(serde_json::json!({
            "ok": false,
            "error": format!("unknown command: {}", other),
            "commands": ["health", "sessions", "tasks", "processes", "approvals", "tool_runs", "checkpoints", "plugins", "snapshot"],
            "json_commands": ["create_task", "run", "update_task", "session_messages", "runtime_snapshot", "runtime_dashboard", "runtime_diagnostics", "runtime_hygiene_preview", "runtime_hygiene_archive", "runtime_task_detail", "runtime_approval_detail", "runtime_checkpoint_detail", "runtime_tasks", "runtime_processes", "runtime_approvals", "runtime_tool_runs", "runtime_checkpoints", "runtime_plugins", "logs", "stop_process", "restart_process", "open_process", "ports", "restore_checkpoint", "approve", "deny", "plugin_preview", "plugin_install", "set_plugin_enabled", "model_info", "set_safety_mode", "pair"],
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
    if (require_remote_auth || command_requires_auth(command)) && !authorize(body)? {
        return Ok(serde_json::json!({
            "ok": false,
            "error": "unauthorized: set MERMAID_DAEMON_TOKEN or include auth.token",
        }));
    }
    // health surfaces the DB path — gated above on TCP (require_remote_auth),
    // free on the local 0600 socket.
    if command == "health" {
        let store = mermaid_cli::runtime::RuntimeStore::open_default()?;
        return Ok(serde_json::json!({
            "ok": true,
            "service": "mermaidd",
            "database": store.path().display().to_string(),
        }));
    }
    match command {
        "create_task" => {
            let store = mermaid_cli::runtime::RuntimeStore::open_default()?;
            let title = str_field(body, "title")?;
            let project_path = str_field(body, "project_path")?;
            let model_id = str_field(body, "model_id")?;
            // F18 (RC-E): tag daemon-created tasks so the startup reconcile may
            // recover them — interactive CLI tasks stay un-owned and are spared.
            let task = store.tasks().create(
                mermaid_cli::runtime::NewTask::new(title, project_path, model_id).daemon_owned(),
            )?;
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
            // F18 (RC-E): this task runs in the daemon process, so tag it
            // daemon-owned — a crash leaving it `Running` is recovered by the
            // next startup reconcile (which leaves un-owned CLI tasks alone).
            let task = store.tasks().create(
                mermaid_cli::runtime::NewTask::new(
                    task_title_from_prompt(&prompt),
                    project_path.display().to_string(),
                    model_id.clone(),
                )
                .daemon_owned(),
            )?;
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
                // Map the run outcome to its terminal status + report.
                let (status, report, hook_status) = match result {
                    Ok(result) if result.errors.is_empty() => (
                        mermaid_cli::runtime::TaskStatus::Completed,
                        result.response,
                        "completed",
                    ),
                    Ok(result) => (
                        mermaid_cli::runtime::TaskStatus::Failed,
                        result.errors.join("\n"),
                        "failed",
                    ),
                    Err(err) => (
                        mermaid_cli::runtime::TaskStatus::Failed,
                        err.to_string(),
                        "failed",
                    ),
                };
                // F20: persist the terminal status DURABLY. The old code
                // `let _`-swallowed this write AND skipped it entirely when the
                // store failed to open — so a truly-finished task stayed `running`
                // and the next daemon reconcile flipped it to `failed`, discarding
                // the real final_report. Retry (reopening the store) and log loudly
                // so the final status + report survive.
                persist_terminal_status(&task_id, status, &report).await;
                let _ = mermaid_cli::runtime::run_plugin_hooks(
                    "task_stop",
                    &serde_json::json!({
                        "id": task_id.clone(),
                        "status": hook_status,
                        "final_report": report.clone(),
                    }),
                );
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
            let preview = mermaid_cli::runtime::plugin_capability_preview(std::path::Path::new(
                str_field(body, "path")?,
            ))?;
            Ok(serde_json::json!({"ok": true, "preview": preview}))
        },
        "plugin_install" => {
            let path = std::path::Path::new(str_field(body, "path")?);
            let preview = mermaid_cli::runtime::plugin_capability_preview(path)?;
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
            let ttl_days = body
                .get("ttl_days")
                .and_then(|v| v.as_i64())
                .unwrap_or(mermaid_cli::runtime::DEFAULT_PAIRING_TTL_DAYS);
            // #65: clamp so a socket caller can't mint a never-expiring token by
            // sending ttl_days <= 0.
            let ttl_days = mermaid_cli::runtime::clamp_pairing_ttl_days(ttl_days);
            let expires_at = mermaid_cli::runtime::pairing_expiry_from_now(ttl_days);
            let (token, hash) = match body.get("token_hash").and_then(|v| v.as_str()) {
                Some(hash) if !hash.is_empty() => (None, hash.to_string()),
                _ => {
                    let (token, hash) = mermaid_cli::runtime::generate_pairing_token()?;
                    (Some(token), hash)
                },
            };
            let record = store
                .pairing_tokens()
                .create(&hash, label, expires_at.as_deref())?;
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
            | "restore_checkpoint"
            | "approve"
            | "deny"
            | "stop_process"
            | "restart_process"
            | "open_process"
            // #67: preview resolves a plugin source, which can `git clone/pull` a
            // remote URL when MERMAID_ALLOW_PLUGIN_FETCH=1 — gate it like install.
            | "plugin_preview"
            | "plugin_install"
            | "set_plugin_enabled"
            | "set_safety_mode"
            | "runtime_hygiene_archive"
            | "pair"
            // Process logs can carry sensitive command output; require the
            // pairing token like the other privileged reads/mutations.
            | "logs"
            // #21 (breaking, consistent with the #66 hardening): the read
            // commands expose session messages, full DB snapshots, and the
            // runtime panels — all of which can carry project/credential
            // content. Gate them behind the pairing token too, so a same-UID
            // process can't read them off the socket without pairing. The
            // in-process `RuntimeClient` (PreferDaemon) simply falls back to a
            // direct local read when the daemon rejects, so the TUI is
            // unaffected; explicit daemon clients must use `daemon_with_token`.
            | "session_messages"
            | "snapshot"
            | "runtime_snapshot"
            | "runtime_dashboard"
            | "runtime_diagnostics"
            | "runtime_hygiene_preview"
            | "runtime_task_detail"
            | "runtime_approval_detail"
            | "runtime_checkpoint_detail"
            | "runtime_tasks"
            | "runtime_processes"
            | "runtime_approvals"
            | "runtime_tool_runs"
            | "runtime_checkpoints"
            | "runtime_plugins"
            | "model_info"
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
    // Constant-time hash match + expiry enforced inside verify_token.
    let Some(record) = store.pairing_tokens().verify_token(&hash)? else {
        return Ok(false);
    };
    store.pairing_tokens().mark_used(&record.id)?;
    Ok(true)
}

/// Durably persist a task's terminal status + report (F20). The daemon's spawned
/// run task is the only writer of this final state; if the write is lost the task
/// is left `running` and the next startup reconcile fails it (discarding the real
/// report). Retry a few times, reopening the store each attempt, and log loudly
/// if it still can't be written rather than swallowing the error.
#[cfg(unix)]
async fn persist_terminal_status(
    task_id: &str,
    status: mermaid_cli::runtime::TaskStatus,
    report: &str,
) {
    const MAX_ATTEMPTS: usize = 3;
    for attempt in 1..=MAX_ATTEMPTS {
        match mermaid_cli::runtime::RuntimeStore::open_default() {
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
            "restore_checkpoint",
            "approve",
            "deny",
            "stop_process",
            "restart_process",
            "open_process",
            "plugin_preview",
            "plugin_install",
            "set_plugin_enabled",
            "set_safety_mode",
            "runtime_hygiene_archive",
            "pair",
            "logs",
            // #21: privileged reads now gated behind the pairing token too —
            // they expose session messages, full DB snapshots, and the runtime
            // panels, any of which can carry project/credential content.
            "session_messages",
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
            "runtime_plugins",
            "model_info",
        ] {
            assert!(super::command_requires_auth(command), "{command}");
        }
        // Liveness/discovery commands stay unauthenticated: they expose no
        // project or credential content, and `health`/`ports` are used for
        // daemon discovery before a token is available.
        for command in ["health", "ports", "version"] {
            assert!(!super::command_requires_auth(command), "{command}");
        }
    }

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
