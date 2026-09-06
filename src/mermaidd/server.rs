//! The listeners (a Unix socket, a Windows named pipe, an optional loopback
//! TCP port) and the per-connection line protocol.

use anyhow::{Context, Result};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

use super::*;

pub(super) const DEFAULT_TCP_ADDR: &str = "127.0.0.1:39871";

#[cfg(unix)]
pub(super) async fn serve_unix() -> Result<()> {
    let data_dir = mermaid_runtime::data_dir()?;
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
    let _daemon_lock = match mermaid_runtime::try_exclusive_lock(&lock_path)
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
pub(super) fn uid_allowed(peer_uid: u32, owner_uid: u32) -> bool {
    peer_uid == owner_uid || peer_uid == 0
}

/// Serve the control plane over a named pipe locked to the owning user. The
/// pipe's DACL (see `pipe_sddl`) is the Windows analog of the 0600 socket +
/// uid peer check: identity is enforced by the kernel at `open` time, so no
/// post-accept peer check is needed. Remote clients are refused via
/// `PIPE_REJECT_REMOTE_CLIENTS` (tokio's default, set explicitly anyway).
#[cfg(windows)]
pub(super) async fn serve_windows() -> Result<()> {
    use tokio::net::windows::named_pipe::ServerOptions;

    let pipe_name = mermaid_runtime::daemon::daemon_pipe_name()?;
    let mut security = mermaid_runtime::daemon::PipeSecurity::owner_only()?;

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

pub(super) async fn maybe_spawn_tcp_listener() {
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
                if let Ok(dir) = mermaid_runtime::data_dir() {
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
                println!("mermaidd tcp listening on {local_addr}");
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

pub(super) async fn handle_stream<S>(stream: S) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    handle_stream_inner(stream, false).await
}

pub(super) async fn handle_remote_stream<S>(stream: S) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    handle_stream_inner(stream, true).await
}

pub(super) async fn handle_stream_inner<S>(stream: S, require_auth: bool) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let timeout =
        std::time::Duration::from_secs(mermaid_model::constants::DAEMON_CONNECTION_TIMEOUT_SECS);
    // Bounded read: a pre-auth client (especially over TCP) must not be able to
    // stream bytes without a newline and grow this buffer without bound (#22).
    // The read itself is inside the connection timeout too.
    let mut reader = BufReader::new(stream);
    let line = match tokio::time::timeout(
        timeout,
        mermaid_model::utils::read_line_capped(
            &mut reader,
            mermaid_model::constants::MAX_DAEMON_COMMAND_BYTES,
        ),
    )
    .await
    .map_err(|_| anyhow::anyhow!("client sent no complete command within the timeout"))??
    {
        mermaid_model::utils::CappedLine::Line(bytes) => {
            String::from_utf8_lossy(&bytes).into_owned()
        },
        mermaid_model::utils::CappedLine::TooLong => {
            anyhow::bail!("daemon command exceeded size cap")
        },
        mermaid_model::utils::CappedLine::Eof => String::new(),
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
pub(super) fn parse_subscribe(line: &str) -> Option<crate::runtime_client::DaemonRequest> {
    if !line.starts_with('{') {
        return None;
    }
    match serde_json::from_str::<crate::runtime_client::DaemonRequest>(line) {
        Ok(req @ crate::runtime_client::DaemonRequest::SubscribeTask { .. }) => Some(req),
        _ => None,
    }
}

/// Subscriptions carry session content, so they're always token-gated on
/// the wire (mirrors `DaemonRequest::requires_auth`).
pub(super) fn request_requires_auth_wire(_line: &str) -> bool {
    true
}
