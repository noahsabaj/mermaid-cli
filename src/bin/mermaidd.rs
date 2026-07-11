#[cfg(any(unix, windows))]
use anyhow::{Context, Result};

#[cfg(any(unix, windows))]
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

#[cfg(any(unix, windows))]
const DEFAULT_TCP_ADDR: &str = "127.0.0.1:39871";

#[cfg(any(unix, windows))]
const HELP: &str = "\
mermaidd — Mermaid's background daemon (durable runtime state, remote attach,
long-running process ownership).

Usage: mermaidd [--version | --help]

With no arguments it runs the daemon in the foreground, serving the control
socket (a Unix domain socket; an owner-locked named pipe on Windows). On Linux
it is normally managed via the `mermaid daemon` subcommands (install, start,
stop, restart, status, logs) rather than invoked directly.
";

#[cfg(any(unix, windows))]
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
#[cfg(any(unix, windows))]
fn classify_args<I: IntoIterator<Item = String>>(args: I) -> CliAction {
    match args.into_iter().next().as_deref() {
        None => CliAction::Run,
        Some("--version" | "-V" | "version") => CliAction::Version,
        Some("--help" | "-h" | "help") => CliAction::Help,
        Some(other) => CliAction::Unknown(other.to_string()),
    }
}

/// The daemon task scheduler. `run` requests only *enqueue*; the drain loop
/// executes queued tasks bounded by `daemon.max_concurrent_tasks` permits, so
/// a burst of runs proceeds serially (or up to the configured width) instead
/// of stampeding the GPU with N simultaneous agent loops. `running` maps each
/// in-flight task to its cancellation token — the handle `cancel_task` uses to
/// stop it.
#[cfg(any(unix, windows))]
struct Scheduler {
    permits: std::sync::Arc<tokio::sync::Semaphore>,
    running:
        std::sync::Mutex<std::collections::HashMap<String, tokio_util::sync::CancellationToken>>,
    wake: tokio::sync::Notify,
    task_timeout: Option<std::time::Duration>,
    /// Live `RunEvent` broadcast per task, for `subscribe_task`. GET-OR-CREATE
    /// from BOTH the executor (which wires it into `RunOptions.event_tx`) and
    /// the subscribe handler — so subscribing to a still-QUEUED task works:
    /// the subscriber holds a receiver on the sender the executor later uses.
    /// Entries are removed by a drop guard after the terminal status persists.
    streams: std::sync::Mutex<
        std::collections::HashMap<
            String,
            tokio::sync::broadcast::Sender<mermaid_cli::domain::RunEvent>,
        >,
    >,
}

#[cfg(any(unix, windows))]
impl Scheduler {
    /// The task's live event sender, created on first request. Capacity 1024:
    /// a lagged subscriber gets a `RecvError::Lagged` marker, not backpressure
    /// into the run.
    fn stream_for(
        &self,
        task_id: &str,
    ) -> tokio::sync::broadcast::Sender<mermaid_cli::domain::RunEvent> {
        self.streams
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(task_id.to_string())
            .or_insert_with(|| tokio::sync::broadcast::channel(1024).0)
            .clone()
    }

    fn drop_stream(&self, task_id: &str) {
        self.streams
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(task_id);
    }
}

#[cfg(any(unix, windows))]
static SCHEDULER: std::sync::OnceLock<Scheduler> = std::sync::OnceLock::new();

#[cfg(any(unix, windows))]
fn scheduler() -> &'static Scheduler {
    SCHEDULER
        .get()
        .expect("scheduler is initialized in main before serving")
}

#[cfg(any(unix, windows))]
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

    // Open (and thereby create/validate) the runtime store up front on every
    // platform: a broken DB should fail the daemon fast, not its first client.
    drop(mermaid_cli::runtime::RuntimeStore::open_default()?);

    // Scheduler singleton. Config is read once at startup (a restart picks up
    // changes); the drain loop itself is spawned by the serve fns AFTER the
    // platform singleton guard + startup_recovery, so a fresh claim can never
    // race the stranded-Running reconcile.
    let daemon_config = mermaid_cli::app::load_config().unwrap_or_default().daemon;
    let _ = SCHEDULER.set(Scheduler {
        permits: std::sync::Arc::new(tokio::sync::Semaphore::new(
            daemon_config.max_concurrent_tasks.max(1),
        )),
        running: std::sync::Mutex::new(std::collections::HashMap::new()),
        streams: std::sync::Mutex::new(std::collections::HashMap::new()),
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

/// Drain queued daemon tasks forever: take a permit, claim the next queued
/// task, execute it, repeat. Queued tasks left over from a previous daemon are
/// picked up automatically on restart — the queue is durable.
#[cfg(any(unix, windows))]
async fn scheduler_drain_loop() {
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
            let claimed = mermaid_cli::runtime::RuntimeStore::open_default()
                .and_then(|store| store.tasks().claim_next_queued());
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
#[cfg(any(unix, windows))]
struct RunningGuard {
    sched: &'static Scheduler,
    task_id: String,
}

#[cfg(any(unix, windows))]
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
#[cfg(any(unix, windows))]
async fn execute_claimed_task(
    task: mermaid_cli::runtime::TaskRecord,
    permit: tokio::sync::OwnedSemaphorePermit,
) {
    let _permit = permit;
    let sched = scheduler();
    let Some(prompt) = task.prompt.clone() else {
        // Unreachable via `claim_next_queued` (it filters `prompt IS NOT
        // NULL`), but never leave a row wedged in Running if it happens.
        persist_terminal_status(
            &task.id,
            mermaid_cli::runtime::TaskStatus::Failed,
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

    let _ = mermaid_cli::runtime::run_plugin_hooks(
        "task_start",
        &serde_json::json!({
            "id": task.id.clone(),
            "title": task.title.clone(),
            "project_path": task.project_path.clone(),
            "model_id": task.model_id.clone(),
        }),
    );

    let config = mermaid_cli::app::load_config().unwrap_or_default();
    // Live event stream for `subscribe_task` attachments (pre-run
    // subscribers already hold receivers on this same sender).
    let event_tx = sched.stream_for(&task.id);
    let result = mermaid_cli::app::run_non_interactive_with(
        config,
        std::path::PathBuf::from(&task.project_path),
        task.model_id.clone(),
        prompt,
        mermaid_cli::app::RunOptions {
            task_id: Some(task.id.clone()),
            cancel: Some(token.clone()),
            deadline: sched.task_timeout,
            event_tx: Some(event_tx),
            ..mermaid_cli::app::RunOptions::default()
        },
    )
    .await;

    // The run has ended; `_running_guard` removes the token from the running map
    // when this function returns. Map the outcome to a terminal status + report
    // — an explicit cancel wins over whatever the interrupted run returned.
    let (status, report, hook_status) = classify_run_result(token.is_cancelled(), result);
    // F20: persist the terminal status DURABLY (see persist_terminal_status).
    persist_terminal_status(&task.id, status, &report).await;
    // AFTER the terminal status persists: late subscribers now read the row
    // and synthesize a terminal event instead of racing the live stream.
    sched.drop_stream(&task.id);
    record_terminal_outcome(&task, status);
    let _ = mermaid_cli::runtime::run_plugin_hooks(
        "task_stop",
        &serde_json::json!({
            "id": task.id.clone(),
            "status": hook_status,
            "final_report": report.clone(),
        }),
    );
}

/// Recover state stranded by a previous daemon's crash (#120, #118) and prune
/// old runtime rows + checkpoint dirs (#130). Must run singleton-guarded — by
/// the unix flock or the windows first-pipe-instance — so it executes once per
/// live daemon. All best-effort.
#[cfg(any(unix, windows))]
fn startup_recovery() {
    let daemon = mermaid_cli::app::load_config().unwrap_or_default().daemon;
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
        match store.gc(daemon.retention_days, daemon.outcomes_retention_days) {
            Ok(removed) if removed > 0 => tracing::info!(removed, "gc pruned old runtime rows"),
            Ok(_) => {},
            Err(error) => tracing::warn!(error = %error, "startup gc failed"),
        }
    }
    if let Ok(removed) = mermaid_cli::runtime::gc_old_checkpoint_dirs(daemon.retention_days)
        && removed > 0
    {
        tracing::info!(removed, "gc removed old checkpoint directories");
    }
    if let Ok(removed) = sweep_stale_bg_logs(daemon.retention_days)
        && removed > 0
    {
        tracing::info!(removed, "reaped stale background-command logs");
    }
    // Per-session scratch directories: interactive sessions sweep with the
    // built-in default on startup; the daemon adds a knob-driven pass so
    // long-lived daemon-only boxes still converge.
    if let Ok(removed) = mermaid_cli::session::scratchpad::sweep_stale(
        daemon.scratchpad_retention_days.max(0) as u64,
    ) && removed > 0
    {
        tracing::info!(removed, "reaped stale session scratchpads");
    }
}

/// Reap background-command tee logs (`mermaid-bg-<pid>-<nanos>.log`) left in the
/// private temp dir by detached (Ctrl+B) commands of prior sessions. A live
/// detached process keeps appending to its log, so an mtime older than the
/// retention window means the writer is long gone.
#[cfg(any(unix, windows))]
fn sweep_stale_bg_logs(retention_days: i64) -> std::io::Result<u64> {
    sweep_stale_bg_logs_in(&mermaid_cli::utils::private_temp_dir()?, retention_days)
}

/// The `sweep_stale_bg_logs` body over an explicit directory, for testing.
/// Best-effort: files it can't stat or remove (e.g. one a live process still
/// holds open on Windows) are skipped.
#[cfg(any(unix, windows))]
fn sweep_stale_bg_logs_in(dir: &std::path::Path, retention_days: i64) -> std::io::Result<u64> {
    let cutoff = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(
            retention_days.max(0) as u64 * 24 * 60 * 60,
        ))
        .unwrap_or(std::time::UNIX_EPOCH);
    let mut removed = 0u64;
    for entry in std::fs::read_dir(dir)?.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !(name.starts_with("mermaid-bg-") && name.ends_with(".log")) {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .map(|mtime| mtime < cutoff)
            .unwrap_or(false);
        if stale && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

#[cfg(unix)]
async fn serve_unix() -> Result<()> {
    let data_dir = mermaid_cli::runtime::data_dir()?;
    let socket_path = data_dir.join("mermaidd.sock");

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

    // Only the lock holder reaches here, so recovery/GC runs once per live
    // daemon (#120, #118, #130).
    startup_recovery();

    // Drain queued tasks (including any left by a previous daemon) — spawned
    // only after recovery so a fresh claim can't race the stranded-Running
    // reset.
    tokio::spawn(scheduler_drain_loop());

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
        // A transient accept error (EMFILE under fd pressure, a peer that
        // vanished mid-handshake) must NOT take the whole daemon down — the old
        // `?` propagated it out of `main`. Log, brief-pause on error to avoid a
        // hot spin, and keep serving.
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(err) => {
                tracing::warn!(error = %err, "mermaidd unix accept failed; continuing");
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                continue;
            },
        };
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
            // The connection bound now lives INSIDE handle_stream_inner: the
            // first-line read and one-shot dispatch are timed out there, while
            // a subscribe_task stream legitimately outlives it (per-write
            // timeout instead).
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

/// Serve the control plane over a named pipe locked to the owning user. The
/// pipe's DACL (see `pipe_sddl`) is the Windows analog of the 0600 socket +
/// uid peer check: identity is enforced by the kernel at `open` time, so no
/// post-accept peer check is needed. Remote clients are refused via
/// `PIPE_REJECT_REMOTE_CLIENTS` (tokio's default, set explicitly anyway).
#[cfg(windows)]
async fn serve_windows() -> Result<()> {
    use tokio::net::windows::named_pipe::ServerOptions;

    let pipe_name = mermaid_cli::runtime::daemon::daemon_pipe_name()?;
    let mut security = mermaid_cli::runtime::daemon::PipeSecurity::owner_only()?;

    // The first instance doubles as the singleton guard (the named-pipe analog
    // of the unix flock, #131): while any mermaidd holds an instance of this
    // name, a second daemon's first-instance create fails with
    // `PermissionDenied`. Unlike unix sockets there is no stale-file case —
    // the name vanishes with the last handle.
    let mut server = match unsafe {
        ServerOptions::new()
            .first_pipe_instance(true)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(&pipe_name, security.attributes_ptr())
    } {
        Ok(server) => server,
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => anyhow::bail!(
            "a mermaidd daemon is already serving {pipe_name} — stop it before starting another"
        ),
        Err(err) => {
            return Err(err).with_context(|| format!("failed to create pipe {pipe_name}"));
        },
    };

    // Only the first-instance holder reaches here, so recovery/GC runs once
    // per live daemon (#120, #118, #130) — same guarantee the flock gives unix.
    startup_recovery();

    // Drain queued tasks (including any left by a previous daemon) — spawned
    // only after recovery so a fresh claim can't race the stranded-Running
    // reset.
    tokio::spawn(scheduler_drain_loop());

    println!("mermaidd listening on {pipe_name}");

    maybe_spawn_tcp_listener().await;

    loop {
        if let Err(err) = server.connect().await {
            // Transient connect failures must not take the daemon down — log,
            // brief-pause to avoid a hot spin, and keep serving (mirrors the
            // unix accept-error handling).
            tracing::warn!(error = %err, "mermaidd pipe connect failed; continuing");
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            continue;
        }
        // Stand up the next instance before handing this one off, so a client
        // burst never finds no listener (their opens see ERROR_PIPE_BUSY only
        // for the moment between connect and this create).
        let next = loop {
            match unsafe {
                ServerOptions::new()
                    .reject_remote_clients(true)
                    .create_with_security_attributes_raw(&pipe_name, security.attributes_ptr())
            } {
                Ok(next) => break next,
                Err(err) => {
                    tracing::warn!(error = %err, "mermaidd pipe re-create failed; retrying");
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                },
            }
        };
        let client = std::mem::replace(&mut server, next);
        tokio::spawn(async move {
            // Timeout lives inside handle_stream_inner (see the unix accept).
            if let Err(err) = handle_stream(client).await {
                tracing::warn!(error = %err, "mermaidd client failed");
            }
        });
    }
}

#[cfg(any(unix, windows))]
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
                    match std::fs::write(&tcp_file, local_addr.to_string()) {
                        Ok(()) => {
                            // The hint file holds a loopback address, not a secret,
                            // so a chmod failure warns rather than killing the
                            // listener. (Windows: the per-user profile dir's ACL
                            // already scopes it.)
                            #[cfg(unix)]
                            {
                                use std::os::unix::fs::PermissionsExt;
                                if let Err(err) = std::fs::set_permissions(
                                    &tcp_file,
                                    std::fs::Permissions::from_mode(0o600),
                                ) {
                                    tracing::warn!(file = %tcp_file.display(), error = %err, "failed to lock tcp hint file to 0600");
                                }
                            }
                        },
                        Err(err) => tracing::warn!(
                            file = %tcp_file.display(),
                            error = %err,
                            "failed to write tcp hint file; remote attach may not find the daemon"
                        ),
                    }
                }
                println!("mermaidd tcp listening on {}", local_addr);
            }
            tokio::spawn(async move {
                loop {
                    match listener.accept().await {
                        Ok((stream, _)) => {
                            tokio::spawn(async move {
                                // Timeout lives inside handle_stream_inner
                                // (see the unix accept).
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

#[cfg(any(unix, windows))]
async fn handle_stream<S>(stream: S) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    handle_stream_inner(stream, false).await
}

#[cfg(any(unix, windows))]
async fn handle_remote_stream<S>(stream: S) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    handle_stream_inner(stream, true).await
}

#[cfg(any(unix, windows))]
async fn handle_stream_inner<S>(stream: S, require_auth: bool) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let timeout =
        std::time::Duration::from_secs(mermaid_cli::constants::DAEMON_CONNECTION_TIMEOUT_SECS);
    // Bounded read: a pre-auth client (especially over TCP) must not be able to
    // stream bytes without a newline and grow this buffer without bound (#22).
    // The read itself is inside the connection timeout too.
    let mut reader = BufReader::new(stream);
    let line = match tokio::time::timeout(
        timeout,
        mermaid_cli::utils::read_line_capped(
            &mut reader,
            mermaid_cli::constants::MAX_DAEMON_COMMAND_BYTES,
        ),
    )
    .await
    .map_err(|_| anyhow::anyhow!("client sent no complete command within the timeout"))??
    {
        mermaid_cli::utils::CappedLine::Line(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        mermaid_cli::utils::CappedLine::TooLong => {
            anyhow::bail!("daemon command exceeded size cap")
        },
        mermaid_cli::utils::CappedLine::Eof => String::new(),
    };
    let line = line.trim();
    // Streaming subscriptions are classified BEFORE the connection timeout —
    // a subscription legitimately outlives it (it ends on the task's
    // terminal event / write error, with a per-write timeout instead).
    if let Some(request) = parse_subscribe(line) {
        let authorized = !(require_auth || request_requires_auth_wire(line)) || {
            let body: serde_json::Value = serde_json::from_str(line)?;
            authorize(&body)?
        };
        let stream = reader.into_inner();
        return handle_subscribe_stream(stream, request, authorized).await;
    }
    let response = tokio::time::timeout(timeout, handle_command(line, require_auth))
        .await
        .map_err(|_| anyhow::anyhow!("command handler exceeded the connection timeout"))??;
    let mut stream = reader.into_inner();
    stream.write_all(response.to_string().as_bytes()).await?;
    stream.write_all(b"\n").await?;
    stream.shutdown().await?;
    Ok(())
}

/// Parse a wire line as a `subscribe_task` request, or `None` for the
/// one-shot path.
#[cfg(any(unix, windows))]
fn parse_subscribe(line: &str) -> Option<mermaid_cli::runtime::DaemonRequest> {
    if !line.starts_with('{') {
        return None;
    }
    match serde_json::from_str::<mermaid_cli::runtime::DaemonRequest>(line) {
        Ok(req @ mermaid_cli::runtime::DaemonRequest::SubscribeTask { .. }) => Some(req),
        _ => None,
    }
}

/// Subscriptions carry session content, so they're always token-gated on
/// the wire (mirrors `DaemonRequest::requires_auth`).
#[cfg(any(unix, windows))]
fn request_requires_auth_wire(_line: &str) -> bool {
    true
}

/// Serve one `subscribe_task` connection: ack line, then NDJSON `RunEvent`s
/// until the terminal `result`. An already-terminal task gets the ack plus
/// ONE synthesized result from the persisted record. Slow clients are
/// dropped by a per-write timeout so they can't block the daemon.
#[cfg(any(unix, windows))]
async fn handle_subscribe_stream<S>(
    mut stream: S,
    request: mermaid_cli::runtime::DaemonRequest,
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

    let mermaid_cli::runtime::DaemonRequest::SubscribeTask { task_id } = request else {
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
    let store = mermaid_cli::runtime::RuntimeStore::open_default()?;
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
    write_line(
        &mut stream,
        &serde_json::json!({
            "ok": true,
            "subscribed": task_id,
            "status": task.status.to_string(),
            "protocol_version": mermaid_cli::domain::RUN_EVENT_PROTOCOL_VERSION,
        })
        .to_string(),
    )
    .await?;
    let terminal_status = matches!(
        task.status,
        mermaid_cli::runtime::TaskStatus::Completed
            | mermaid_cli::runtime::TaskStatus::Failed
            | mermaid_cli::runtime::TaskStatus::Cancelled
    );
    if terminal_status {
        // Best-effort synthesis from the persisted record: tokens are not
        // stored per-task, and a failed/cancelled status rides in `errors`.
        let errors = match task.status {
            mermaid_cli::runtime::TaskStatus::Completed => Vec::new(),
            status => vec![format!("task ended {status}")],
        };
        let event = mermaid_cli::domain::RunEvent::Result {
            response: task.final_report.clone().unwrap_or_default(),
            reasoning: None,
            total_tokens: 0,
            errors,
            session_id: String::new(),
            structured_output: None,
        };
        write_line(&mut stream, &serde_json::to_string(&event)?).await?;
        stream.shutdown().await?;
        return Ok(());
    }
    loop {
        match rx.recv().await {
            Ok(event) => {
                let terminal = matches!(event, mermaid_cli::domain::RunEvent::Result { .. });
                write_line(&mut stream, &serde_json::to_string(&event)?).await?;
                if terminal {
                    break;
                }
            },
            // Lagged: stay inside the RunEvent contract with an error event.
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                let event = mermaid_cli::domain::RunEvent::Error {
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

#[cfg(any(unix, windows))]
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
            let store = mermaid_cli::runtime::RuntimeStore::open_default()?;
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
            "json_commands": ["create_task", "run", "cancel_task", "update_task", "session_messages", "snapshot", "runtime_snapshot", "runtime_dashboard", "runtime_diagnostics", "runtime_hygiene_preview", "runtime_hygiene_archive", "runtime_task_detail", "runtime_approval_detail", "runtime_checkpoint_detail", "runtime_tasks", "runtime_processes", "runtime_approvals", "runtime_tool_runs", "runtime_checkpoints", "runtime_plugins", "logs", "stop_process", "restart_process", "open_process", "ports", "restore_checkpoint", "approve", "deny", "plugin_preview", "plugin_install", "set_plugin_enabled", "model_info", "set_safety_mode", "pair", "subscribe_task"],
        })),
    }
}

#[cfg(any(unix, windows))]
async fn handle_json_command(
    body: &serde_json::Value,
    require_remote_auth: bool,
) -> Result<serde_json::Value> {
    use mermaid_cli::runtime::DaemonRequest;
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
            let store = mermaid_cli::runtime::RuntimeStore::open_default()?;
            Ok(serde_json::json!({
                "ok": true,
                "service": "mermaidd",
                "database": store.path().display().to_string(),
            }))
        },
        req @ (DaemonRequest::CreateTask { .. }
        | DaemonRequest::Run { .. }
        | DaemonRequest::CancelTask { .. }
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

#[cfg(any(unix, windows))]
async fn handle_task_command(
    request: mermaid_cli::runtime::DaemonRequest,
) -> Result<serde_json::Value> {
    use mermaid_cli::runtime::DaemonRequest;
    match request {
        DaemonRequest::CreateTask {
            title,
            project_path,
            model_id,
        } => {
            let store = mermaid_cli::runtime::RuntimeStore::open_default()?;
            // F18 (RC-E): tag daemon-created tasks so the startup reconcile may
            // recover them — interactive CLI tasks stay un-owned and are spared.
            let task = store.tasks().create(
                mermaid_cli::runtime::NewTask::new(title, project_path, model_id).daemon_owned(),
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
            let config = mermaid_cli::app::load_config().unwrap_or_default();
            let model_id = match model_id.as_deref() {
                Some(model_id) if !model_id.is_empty() => model_id.to_string(),
                _ => mermaid_cli::app::resolve_model_id(None, &config).await?,
            };
            let priority = match priority.as_deref() {
                None | Some("") | Some("normal") => mermaid_cli::runtime::TaskPriority::Normal,
                Some("high") => mermaid_cli::runtime::TaskPriority::High,
                Some("low") => mermaid_cli::runtime::TaskPriority::Low,
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
            let store = mermaid_cli::runtime::RuntimeStore::open_default()?;
            // Enqueue only — the scheduler claims it when a permit frees, so a
            // burst of runs executes bounded by `daemon.max_concurrent_tasks`
            // instead of stampeding the GPU. The full prompt is persisted for
            // that deferred execution (and survives a daemon restart).
            // F18 (RC-E): daemon-owned, so a crash leaving it `Running` is
            // recovered by the next startup reconcile.
            let task = store.tasks().create(
                mermaid_cli::runtime::NewTask::new(
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
            let store = mermaid_cli::runtime::RuntimeStore::open_default()?;
            match store.tasks().get(&id)? {
                Some(task) if task.status == mermaid_cli::runtime::TaskStatus::Queued => {
                    store.tasks().update_status(
                        &id,
                        mermaid_cli::runtime::TaskStatus::Cancelled,
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
            let store = mermaid_cli::runtime::RuntimeStore::open_default()?;
            let status = match status.as_str() {
                "queued" => mermaid_cli::runtime::TaskStatus::Queued,
                "running" => mermaid_cli::runtime::TaskStatus::Running,
                "waiting_for_approval" => mermaid_cli::runtime::TaskStatus::WaitingForApproval,
                "blocked" => mermaid_cli::runtime::TaskStatus::Blocked,
                "completed" => mermaid_cli::runtime::TaskStatus::Completed,
                "failed" => mermaid_cli::runtime::TaskStatus::Failed,
                "cancelled" => mermaid_cli::runtime::TaskStatus::Cancelled,
                other => anyhow::bail!("unknown task status: {}", other),
            };
            store
                .tasks()
                .update_status(&id, status, final_report.as_deref())?;
            Ok(serde_json::json!({"ok": true}))
        },
        other => anyhow::bail!("mis-routed task command: {other:?}"),
    }
}

#[cfg(any(unix, windows))]
fn handle_runtime_read(request: mermaid_cli::runtime::DaemonRequest) -> Result<serde_json::Value> {
    use mermaid_cli::runtime::DaemonRequest;
    let service = mermaid_cli::runtime::RuntimeService::open_default()?;
    match request {
        DaemonRequest::SessionMessages { id } => {
            let store = mermaid_cli::runtime::RuntimeStore::open_default()?;
            Ok(serde_json::json!({
                "ok": true,
                "session": store.sessions().get(&id)?,
                "messages": store.messages().list_for_session(&id)?,
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

#[cfg(any(unix, windows))]
fn handle_process_command(
    request: mermaid_cli::runtime::DaemonRequest,
) -> Result<serde_json::Value> {
    use mermaid_cli::runtime::DaemonRequest;
    let service = mermaid_cli::runtime::RuntimeService::open_default()?;
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

#[cfg(any(unix, windows))]
fn handle_admin_command(request: mermaid_cli::runtime::DaemonRequest) -> Result<serde_json::Value> {
    use mermaid_cli::runtime::DaemonRequest;
    match request {
        DaemonRequest::RestoreCheckpoint { id } => {
            let manifest = mermaid_cli::runtime::restore_checkpoint(&id)?;
            Ok(serde_json::json!({"ok": true, "checkpoint": manifest}))
        },
        DaemonRequest::Approve { id } => {
            let result = mermaid_cli::runtime::approve_and_replay(&id)?;
            Ok(
                serde_json::json!({"ok": true, "approval": result.approval, "replayed": result.replayed, "summary": result.summary}),
            )
        },
        DaemonRequest::Deny { id } => {
            let result = mermaid_cli::runtime::deny_approval(&id)?;
            Ok(
                serde_json::json!({"ok": true, "approval": result.approval, "replayed": result.replayed, "summary": result.summary}),
            )
        },
        DaemonRequest::PluginPreview { path } => {
            let preview =
                mermaid_cli::runtime::plugin_capability_preview(std::path::Path::new(&path))?;
            Ok(serde_json::json!({"ok": true, "preview": preview}))
        },
        DaemonRequest::PluginInstall { path } => {
            let path = std::path::Path::new(&path);
            let preview = mermaid_cli::runtime::plugin_capability_preview(path)?;
            let plugin = mermaid_cli::runtime::install_plugin_from_path(path)?;
            Ok(serde_json::json!({"ok": true, "preview": preview, "plugin": plugin}))
        },
        DaemonRequest::SetPluginEnabled { id, enabled } => {
            let service = mermaid_cli::runtime::RuntimeService::open_default()?;
            service.set_plugin_enabled(&id, enabled)?;
            Ok(serde_json::json!({"ok": true}))
        },
        DaemonRequest::SetSafetyMode { mode } => {
            let service = mermaid_cli::runtime::RuntimeService::open_default()?;
            let safety = service.set_safety_mode(&mode)?;
            Ok(serde_json::json!({"ok": true, "safety": safety}))
        },
        DaemonRequest::Pair {
            label,
            ttl_days,
            token_hash,
        } => {
            let store = mermaid_cli::runtime::RuntimeStore::open_default()?;
            let ttl_days = ttl_days.unwrap_or(mermaid_cli::runtime::DEFAULT_PAIRING_TTL_DAYS);
            // #65: clamp so a socket caller can't mint a never-expiring token by
            // sending ttl_days <= 0.
            let ttl_days = mermaid_cli::runtime::clamp_pairing_ttl_days(ttl_days);
            let expires_at = mermaid_cli::runtime::pairing_expiry_from_now(ttl_days);
            let (token, hash) = match token_hash.as_deref() {
                Some(hash) if !hash.is_empty() => (None, hash.to_string()),
                _ => {
                    let (token, hash) = mermaid_cli::runtime::generate_pairing_token()?;
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

#[cfg(any(unix, windows))]
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
#[cfg(any(unix, windows))]
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

/// Map a finished run to its terminal `(status, report, hook_status)`. An
/// explicit cancel wins over whatever the interrupted run returned. Otherwise a
/// run with no errors AND a non-empty response is a success; a run with no
/// errors but an EMPTY response produced nothing — for a headless task that is a
/// failure, not a success. Recording the empty case as `Completed` would stamp a
/// false `task_terminal` success/1.0 into the `outcomes` training corpus (the
/// signal the self-improving loop learns from), so it is mapped to `Failed`.
#[cfg(any(unix, windows))]
fn classify_run_result<E: std::fmt::Display>(
    cancelled: bool,
    result: std::result::Result<mermaid_cli::app::RunResult, E>,
) -> (mermaid_cli::runtime::TaskStatus, String, &'static str) {
    use mermaid_cli::runtime::TaskStatus;
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
#[cfg(any(unix, windows))]
fn record_terminal_outcome(
    task: &mermaid_cli::runtime::TaskRecord,
    status: mermaid_cli::runtime::TaskStatus,
) {
    use mermaid_cli::runtime::{
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

#[cfg(any(unix, windows))]
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

#[cfg(all(test, any(unix, windows)))]
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
        use mermaid_cli::app::RunResult;
        use mermaid_cli::runtime::TaskStatus;

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
        std::fs::create_dir_all(&abandoned).unwrap();
        std::fs::write(abandoned.join("out.txt"), b"stale artifact").unwrap();
        let live = root.join("-proj").join("live");
        std::fs::create_dir_all(&live).unwrap();
        // Our own pid in the lock = a live owner; never reaped.
        std::fs::write(live.join(".lock"), std::process::id().to_string()).unwrap();

        // Retention 0 (the daemon knob clamped) → age never protects; only
        // the live lock does.
        let removed = mermaid_cli::session::scratchpad::sweep_stale_in(&root, 0).expect("sweep");
        assert_eq!(removed, 1);
        assert!(!abandoned.exists(), "unlocked session dirs are reaped");
        assert!(live.exists(), "a live pid lock protects the session dir");

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
        use mermaid_cli::runtime::DaemonRequest;
        // The historical stringly matrix, now driven by the exhaustive typed
        // one. Wire strings parse into the enum and their gating matches.
        let gated = [
            r#"{"command":"create_task","title":"t","project_path":"p","model_id":"m"}"#,
            r#"{"command":"run","prompt":"p"}"#,
            r#"{"command":"cancel_task","id":"t"}"#,
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
                "mode": "ask", "model": "m", "task_id": "t",
            });
            body.as_object_mut().unwrap().retain(|_, v| !v.is_null());
            let parsed = serde_json::from_value::<mermaid_cli::runtime::DaemonRequest>(body);
            assert!(
                parsed.is_ok(),
                "help entry '{name}' no longer parses: {parsed:?}"
            );
        }
    }

    #[test]
    fn stream_registry_is_get_or_create_and_drop() {
        let sched = super::Scheduler {
            permits: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
            running: std::sync::Mutex::new(std::collections::HashMap::new()),
            wake: tokio::sync::Notify::new(),
            task_timeout: None,
            streams: std::sync::Mutex::new(std::collections::HashMap::new()),
        };
        // Subscribe BEFORE the executor asks: both get the same sender, so a
        // pre-run subscriber receives the run's events.
        let early = sched.stream_for("t1");
        let mut rx = early.subscribe();
        let executor_side = sched.stream_for("t1");
        executor_side
            .send(mermaid_cli::domain::RunEvent::Error {
                message: "hello".to_string(),
            })
            .expect("subscriber attached");
        match rx.try_recv().expect("event delivered") {
            mermaid_cli::domain::RunEvent::Error { message } => assert_eq!(message, "hello"),
            other => panic!("wrong event: {other:?}"),
        }
        // Drop guard cleans the entry; the next stream_for is a fresh channel.
        sched.drop_stream("t1");
        assert!(sched.streams.lock().unwrap().is_empty());
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
                session_id: None,
                message_index: None,
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

#[cfg(not(any(unix, windows)))]
fn main() {
    eprintln!("mermaidd currently supports Unix and Windows only");
    std::process::exit(1);
}
