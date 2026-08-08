//! Auto-managed local SearXNG for zero-config `web_search`.
//!
//! When `web_search` resolves to the managed backend — the sovereign default
//! (`[web] search_backend = "auto"`, independent of cloud credentials) — the
//! first search lazily provisions a self-contained SearXNG bundle (a portable
//! CPython + the Granian server + the SearXNG app, downloaded and
//! sha256-verified from the `mermaid-searxng` releases; see [`bundle`]) and
//! spawns Granian bound to
//! `127.0.0.1:<port>`. No container runtime — no Docker, no Podman, no VM. The
//! server is torn down when the top-level `EffectRunner` shuts down (i.e. mermaid
//! exits), the same reap path as MCP servers. The verified runtime bundle is
//! cached between sessions; the process and its private settings are not.

pub mod bundle;
pub mod bundle_manifest;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use tokio::sync::Mutex;

pub use bundle::managed_backend_viability;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);
const HEALTH_TIMEOUT: Duration = Duration::from_secs(3);
const HEALTH_CACHE_TTL: Duration = Duration::from_secs(10);
const HEALTH_BODY_MAX_BYTES: usize = 256 * 1024;
const READY_STABILITY_DELAY: Duration = Duration::from_millis(50);
const REAP_TIMEOUT: Duration = Duration::from_secs(5);
const REAP_POLL_INTERVAL: Duration = Duration::from_millis(25);
const DROP_REAP_TIMEOUT: Duration = Duration::from_secs(2);

static SETTINGS_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A started SearXNG server: the Granian process (spawned as a process-group
/// leader, so `stop()` can reap it *and* its forked workers) and where to reach
/// it.
struct Running {
    child: Child,
    base_url: String,
    runtime: PathBuf,
    ready: bool,
    started_at: Instant,
    last_health_check: Option<Instant>,
    _settings: SettingsFile,
}

enum ExistingAction {
    Return(String),
    Clear { invalidate: Option<PathBuf> },
    Terminate { invalidate: Option<PathBuf> },
}

impl Running {
    fn health_is_fresh(&self) -> bool {
        health_check_is_fresh(self.last_health_check)
    }
}

fn health_check_is_fresh(last_health_check: Option<Instant>) -> bool {
    last_health_check.is_some_and(|checked| checked.elapsed() <= HEALTH_CACHE_TTL)
}

impl Drop for Running {
    fn drop(&mut self) {
        match self.child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => {},
            Err(error) => {
                tracing::warn!(%error, "could not inspect managed SearXNG during owner drop");
                return;
            },
        }

        // Normal paths retain the record across async termination and only drop
        // it after `try_wait` reaps the child. This is the last-resort guard for
        // panics or future refactors: a dropped owner must not orphan Granian's
        // process group.
        mermaid_model::utils::terminate_tree_blocking(
            self.child.id(),
            mermaid_model::utils::Grace::Immediate,
        );
        let deadline = Instant::now() + DROP_REAP_TIMEOUT;
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) | Err(_) => return,
                Ok(None) => std::thread::sleep(REAP_POLL_INTERVAL),
            }
        }
        tracing::warn!(
            pid = self.child.id(),
            "managed SearXNG was terminated during owner drop but could not be reaped"
        );
    }
}

struct SettingsFile {
    path: PathBuf,
}

impl Drop for SettingsFile {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_file(&self.path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(path = %self.path.display(), %error, "could not remove managed SearXNG settings");
        }
    }
}

#[derive(Default)]
pub struct SearxngManager {
    /// `None` until the server is up. The `Mutex` serializes concurrent
    /// first-searches so exactly one server starts.
    running: Mutex<Option<Running>>,
}

static MANAGER: OnceLock<SearxngManager> = OnceLock::new();

/// Process-global manager handle (mirrors the MCP manager's `'static` global).
pub fn manager() -> &'static SearxngManager {
    MANAGER.get_or_init(SearxngManager::default)
}

/// Tear down the managed server if one was started. Called from the top-level
/// `EffectRunner::shutdown` on mermaid exit; no-op otherwise.
pub async fn shutdown() {
    if let Some(mgr) = MANAGER.get() {
        mgr.stop().await;
    }
}

impl SearxngManager {
    /// Ensure a SearXNG server is running; return its base URL. The first call
    /// provisions the bundle (download + verify + unpack on a version miss),
    /// spawns Granian, and waits until its JSON API answers; later calls return
    /// the cached URL for the rest of the process.
    pub async fn ensure_running(&self) -> Result<String> {
        let mut running = self.running.lock().await;
        if let Some(base_url) = reconcile_existing(&mut running).await? {
            return Ok(base_url);
        }

        // Ensure the portable CPython + Granian + SearXNG bundle is unpacked; on
        // an unsupported platform this errors with the fallback hint.
        let runtime = bundle::ensure_bundle().await?;
        let reserved = reserve_port()?;
        let port = reserved.port();
        let settings = write_settings()?;
        let child = match spawn_granian(&runtime, reserved, &settings.path) {
            Ok(child) => child,
            Err(error) => {
                let rejection = bundle::invalidate_runtime(&runtime).err();
                let suffix = rejection
                    .map(|error| format!("; rejecting the broken generation also failed ({error})"))
                    .unwrap_or_default();
                return Err(anyhow!(
                    "could not start the SearXNG server (Granian): {error}{suffix}"
                ));
            },
        };
        let base_url = format!("http://127.0.0.1:{port}");

        // Store ownership before the first await. If the caller cancels while
        // readiness is polling, the process handle remains in the manager for
        // the next ensure/shutdown call instead of being dropped and orphaned.
        *running = Some(Running {
            child,
            base_url: base_url.clone(),
            runtime: runtime.clone(),
            ready: false,
            started_at: Instant::now(),
            last_health_check: None,
            _settings: settings,
        });

        let ready = wait_ready(
            running
                .as_mut()
                .expect("the spawned SearXNG process was stored above"),
        )
        .await;
        if let Err(error) = ready {
            let rejection = error
                .runtime_suspect
                .then(|| bundle::invalidate_runtime(&runtime))
                .transpose()
                .err();
            let cleanup =
                terminate_owned(&mut running, mermaid_model::utils::Grace::Immediate).await;
            let mut message = format!("SearXNG did not become ready: {error}");
            if let Some(rejection_error) = rejection {
                message.push_str(&format!(
                    "; rejecting the broken generation also failed ({rejection_error})"
                ));
            }
            if let Err(cleanup_error) = cleanup {
                message.push_str(&format!("; cleanup also failed ({cleanup_error})"));
            }
            return Err(anyhow!(message));
        }
        running
            .as_mut()
            .expect("the ready SearXNG process is still owned")
            .ready = true;
        running
            .as_mut()
            .expect("the ready SearXNG process is still owned")
            .last_health_check = Some(Instant::now());
        Ok(base_url)
    }

    async fn stop(&self) {
        let mut running = self.running.lock().await;
        // Keep the record in the manager across every await. Cancellation can
        // interrupt shutdown, but it cannot discard the only child handle; a
        // later shutdown or ensure call can finish the reap.
        if let Err(error) =
            terminate_owned(&mut running, mermaid_model::utils::Grace::Graceful).await
        {
            tracing::warn!(%error, "could not fully reap managed SearXNG");
        }
    }
}

async fn inspect_existing(existing: &mut Running) -> Result<ExistingAction> {
    Ok(match existing.child.try_wait() {
        Ok(Some(status)) => {
            tracing::warn!(%status, "managed SearXNG exited; restarting it");
            ExistingAction::Clear {
                invalidate: (!existing.ready).then(|| existing.runtime.clone()),
            }
        },
        Ok(None) if !existing.ready => match wait_ready(existing).await {
            Ok(()) => {
                existing.ready = true;
                existing.last_health_check = Some(Instant::now());
                ExistingAction::Return(existing.base_url.clone())
            },
            Err(error) => {
                tracing::warn!(
                    %error,
                    "managed SearXNG startup was interrupted or failed; restarting it"
                );
                ExistingAction::Terminate {
                    invalidate: error.runtime_suspect.then(|| existing.runtime.clone()),
                }
            },
        },
        Ok(None) if existing.health_is_fresh() => ExistingAction::Return(existing.base_url.clone()),
        Ok(None) => return inspect_existing_health(existing).await,
        Err(error) => {
            tracing::warn!(%error, "could not inspect managed SearXNG; restarting it");
            ExistingAction::Terminate { invalidate: None }
        },
    })
}

async fn inspect_existing_health(existing: &mut Running) -> Result<ExistingAction> {
    let healthy = probe_liveness(&existing.base_url).await?;
    Ok(match existing.child.try_wait() {
        Ok(Some(status)) => {
            tracing::warn!(%status, "managed SearXNG exited during its health probe");
            ExistingAction::Clear { invalidate: None }
        },
        Ok(None) if healthy => {
            existing.last_health_check = Some(Instant::now());
            ExistingAction::Return(existing.base_url.clone())
        },
        Ok(None) => {
            tracing::warn!(
                url = %existing.base_url,
                "managed SearXNG failed its JSON search health probe; restarting it"
            );
            ExistingAction::Terminate { invalidate: None }
        },
        Err(error) => {
            tracing::warn!(%error, "could not inspect managed SearXNG after its health probe");
            ExistingAction::Terminate { invalidate: None }
        },
    })
}

async fn reconcile_existing(running: &mut Option<Running>) -> Result<Option<String>> {
    let Some(existing) = running.as_mut() else {
        return Ok(None);
    };
    let action = inspect_existing(existing).await?;
    match action {
        ExistingAction::Return(base_url) => Ok(Some(base_url)),
        ExistingAction::Clear { invalidate } => {
            let rejection = invalidate
                .map(|runtime| bundle::invalidate_runtime(&runtime))
                .transpose();
            // `try_wait` already reaped this child.
            *running = None;
            rejection?;
            Ok(None)
        },
        ExistingAction::Terminate { invalidate } => {
            let rejection = invalidate
                .map(|runtime| bundle::invalidate_runtime(&runtime))
                .transpose();
            let cleanup = terminate_owned(running, mermaid_model::utils::Grace::Immediate).await;
            match (rejection, cleanup) {
                (Ok(_), Ok(())) => Ok(None),
                (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
                (Err(rejection_error), Err(cleanup_error)) => Err(anyhow!(
                    "rejecting the broken SearXNG generation failed ({rejection_error}); cleanup also failed ({cleanup_error})"
                )),
            }
        },
    }
}

async fn terminate_owned(
    running: &mut Option<Running>,
    grace: mermaid_model::utils::Grace,
) -> Result<()> {
    let Some(process) = running.as_mut() else {
        return Ok(());
    };
    let pid = match process.child.try_wait() {
        Ok(Some(_)) => {
            *running = None;
            return Ok(());
        },
        Ok(None) => process.child.id(),
        Err(error) => {
            return Err(anyhow!(
                "could not inspect the managed SearXNG process before termination: {error}"
            ));
        },
    };
    mermaid_model::utils::terminate_tree(pid, grace).await;

    let deadline = Instant::now() + REAP_TIMEOUT;
    loop {
        let child = &mut running
            .as_mut()
            .expect("the process remains owned until it is reaped")
            .child;
        match child.try_wait() {
            Ok(Some(_)) => {
                *running = None;
                return Ok(());
            },
            Ok(None) if Instant::now() < deadline => {
                tokio::time::sleep(REAP_POLL_INTERVAL).await;
            },
            Ok(None) => {
                return Err(anyhow!(
                    "the managed SearXNG process did not exit after {} seconds",
                    REAP_TIMEOUT.as_secs()
                ));
            },
            Err(error) => {
                return Err(anyhow!(
                    "could not reap the managed SearXNG process: {error}"
                ));
            },
        }
    }
}

/// Keep an ephemeral port bound while provisioning settings and building the
/// child command. Granian cannot currently inherit a listener, so
/// `spawn_granian` releases this reservation immediately before `spawn`; there
/// is no await or unrelated filesystem work in the remaining bind window.
struct ReservedPort {
    listener: std::net::TcpListener,
    port: u16,
}

impl ReservedPort {
    fn port(&self) -> u16 {
        self.port
    }
}

fn reserve_port() -> Result<ReservedPort> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .map_err(|e| anyhow!("could not find a free port for SearXNG: {e}"))?;
    let port = listener.local_addr()?.port();
    Ok(ReservedPort { listener, port })
}

/// The bundle's Python interpreter. python-build-standalone lays it out as
/// `python/bin/python3` on unix and `python/python.exe` on Windows; launching
/// Granian through it keeps the relocated tree self-contained (no venv, no baked
/// shebangs).
fn python_bin(runtime: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        runtime.join("python").join("python.exe")
    }
    #[cfg(not(windows))]
    {
        runtime.join("python").join("bin").join("python3")
    }
}

/// Spawn Granian serving the SearXNG WSGI app, bound to loopback on `port`, as a
/// detached process-group leader (so the TUI's Ctrl+C doesn't reach it and
/// `stop()` can group-reap it). Unlike the Ollama server, this child IS reaped on
/// mermaid exit.
fn spawn_granian(
    runtime: &Path,
    reserved: ReservedPort,
    settings: &Path,
) -> std::io::Result<Child> {
    let python = python_bin(runtime);
    let port_str = reserved.port().to_string();
    let mut cmd = std::process::Command::new(&python);
    cmd.args([
        "-m",
        "granian",
        "--interface",
        "wsgi",
        "--host",
        "127.0.0.1",
        "--port",
        port_str.as_str(),
        "searx.webapp:application",
    ])
    .env("SEARXNG_SETTINGS_PATH", settings)
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW, never DETACHED_PROCESS: the latter leaves a
        // visible console window on Windows 11 (see utils::proc).
        cmd.creation_flags(
            mermaid_model::utils::CREATE_NO_WINDOW | mermaid_model::utils::CREATE_NEW_PROCESS_GROUP,
        );
    }
    // Granian has no stable inherited-listener interface in the pinned bundle.
    // Release as late as possible; `wait_ready` additionally requires a live
    // owned child and a valid bounded SearXNG JSON response before trusting it.
    drop(reserved.listener);
    cmd.spawn()
}

/// Write the SearXNG settings that enable the JSON API and disable the bot
/// limiter and the (optional) Valkey cache — all safe for a private localhost
/// instance — returning the owner of a unique file to pass as
/// `SEARXNG_SETTINGS_PATH`. Regenerated each launch so a settings-schema change
/// always propagates and concurrent Mermaid processes cannot overwrite settings
/// during startup.
///
/// Written under the DATA dir (beside the unpacked runtime), not the config dir:
/// the old container backend mounted `<config_dir>/searxng` with `:Z`, which on
/// SELinux systems relabels it to a container-private context the normal process
/// can no longer write. The data dir was never container-touched.
fn write_settings() -> Result<SettingsFile> {
    let dir = mermaid_runtime::data_dir()?.join("searxng");
    write_settings_in(&dir)
}

fn write_settings_in(dir: &Path) -> Result<SettingsFile> {
    write_settings_in_with(dir, random_secret)
}

fn write_settings_in_with(
    dir: &Path,
    mut next_secret: impl FnMut() -> Result<String>,
) -> Result<SettingsFile> {
    std::fs::create_dir_all(dir)?;
    let server_secret = next_secret().context("generating the managed SearXNG server secret")?;
    for _ in 0..32 {
        let sequence = SETTINGS_COUNTER.fetch_add(1, Ordering::Relaxed);
        let filename_secret =
            next_secret().context("generating the managed SearXNG settings filename")?;
        let settings = dir.join(format!(
            "settings-{}-{sequence:x}-{}.yml",
            std::process::id(),
            filename_secret
        ));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&settings) {
            Ok(mut file) => {
                if let Err(error) = file
                    .write_all(settings_yml(&server_secret).as_bytes())
                    .and_then(|()| file.sync_all())
                {
                    drop(file);
                    let _ = std::fs::remove_file(&settings);
                    return Err(error.into());
                }
                return Ok(SettingsFile { path: settings });
            },
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {},
            Err(error) => return Err(error.into()),
        }
    }
    Err(anyhow!(
        "could not allocate a unique settings file for managed SearXNG"
    ))
}

fn settings_yml(server_secret: &str) -> String {
    // ahmia + torch are onion engines that cannot work without a Tor proxy and
    // fail registration at every startup, spamming ERROR logs; removing them
    // from the defaults keeps the log clean at zero functional cost.
    format!(
        "use_default_settings:\n\
        \x20 engines:\n\
        \x20   remove:\n\
        \x20     - ahmia\n\
        \x20     - torch\n\
         server:\n\
        \x20 secret_key: \"{}\"\n\
        \x20 limiter: false\n\
         search:\n\
        \x20 formats:\n\
        \x20   - html\n\
        \x20   - json\n\
         valkey:\n\
        \x20 url: false\n",
        server_secret
    )
}

fn random_secret() -> Result<String> {
    random_secret_with(|bytes| {
        getrandom::fill(bytes)
            .map_err(|error| anyhow!("operating-system randomness is unavailable: {error}"))
    })
}

fn random_secret_with(fill: impl FnOnce(&mut [u8]) -> Result<()>) -> Result<String> {
    let mut bytes = [0u8; 16];
    fill(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Poll the JSON search endpoint until it answers, the server dies, or the
/// cold-start budget passes. Granian import + engine load takes a few seconds on
/// the first request (there is no image pull to wait on any more).
#[derive(Debug)]
struct ReadinessError {
    message: String,
    runtime_suspect: bool,
}

impl ReadinessError {
    fn operational(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            runtime_suspect: false,
        }
    }

    fn runtime(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            runtime_suspect: true,
        }
    }
}

impl std::fmt::Display for ReadinessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ReadinessError {}

async fn wait_ready(running: &mut Running) -> std::result::Result<(), ReadinessError> {
    let client = health_client().map_err(|error| {
        ReadinessError::operational(format!("could not build the health client: {error}"))
    })?;
    let deadline = running.started_at + STARTUP_TIMEOUT;
    loop {
        // Check ownership both before and after probing. This prevents a 2xx
        // response from an unrelated process in the narrow release-to-spawn
        // port window from being accepted as our managed backend.
        match running.child.try_wait() {
            Ok(Some(status)) => {
                return Err(ReadinessError::runtime(format!(
                    "the Granian server exited immediately ({status})"
                )));
            },
            Ok(None) => {},
            Err(error) => {
                return Err(ReadinessError::operational(format!(
                    "could not inspect the Granian server during startup: {error}"
                )));
            },
        }
        let healthy = probe_ready_with(&client, &running.base_url)
            .await
            .map_err(|error| {
                ReadinessError::operational(format!(
                    "could not construct the SearXNG health challenge: {error}"
                ))
            })?;
        match running.child.try_wait() {
            Ok(Some(status)) => {
                return Err(ReadinessError::runtime(format!(
                    "the Granian server exited during its health probe ({status})"
                )));
            },
            Ok(None) => {},
            Err(error) => {
                return Err(ReadinessError::operational(format!(
                    "could not inspect the Granian server after its health probe: {error}"
                )));
            },
        }
        if healthy {
            // A bind collision normally makes Granian exit immediately. Give
            // that failure one scheduling interval to become observable before
            // declaring an independently responding port owned.
            tokio::time::sleep(READY_STABILITY_DELAY).await;
            match running.child.try_wait() {
                Ok(None) => return Ok(()),
                Ok(Some(status)) => {
                    return Err(ReadinessError::runtime(format!(
                        "the Granian server exited immediately after readiness ({status})"
                    )));
                },
                Err(error) => {
                    return Err(ReadinessError::operational(format!(
                        "could not confirm the Granian server after readiness: {error}"
                    )));
                },
            }
        }
        if Instant::now() >= deadline {
            return Err(ReadinessError::operational(format!(
                "no response from {} after startup",
                running.base_url
            )));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn probe_liveness(base_url: &str) -> Result<bool> {
    let client = health_client()?;
    probe_ready_with(&client, base_url).await
}

fn health_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(HEALTH_TIMEOUT)
        .build()
        .map_err(Into::into)
}

#[cfg(test)]
async fn probe_ready(base_url: &str) -> Result<bool> {
    let client = health_client()?;
    probe_ready_with(&client, base_url).await
}

async fn probe_ready_with(client: &reqwest::Client, base_url: &str) -> Result<bool> {
    probe_ready_with_secret(client, base_url, random_secret).await
}

async fn probe_ready_with_secret(
    client: &reqwest::Client,
    base_url: &str,
    next_secret: impl FnOnce() -> Result<String>,
) -> Result<bool> {
    let challenge = format!("mermaid-health-{}", next_secret()?);
    let mut probe = reqwest::Url::parse(base_url).context("parsing the SearXNG health URL")?;
    probe.set_path("/search");
    probe.set_query(None);
    probe
        .query_pairs_mut()
        .append_pair("q", &challenge)
        .append_pair("format", "json");
    Ok(request_succeeded(client, probe, &challenge).await)
}

async fn request_succeeded(
    client: &reqwest::Client,
    url: reqwest::Url,
    expected_query: &str,
) -> bool {
    use futures::StreamExt;

    let Ok(response) = client.get(url).send().await else {
        return false;
    };
    if !response.status().is_success()
        || response
            .content_length()
            .is_some_and(|length| length > HEALTH_BODY_MAX_BYTES as u64)
    {
        return false;
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else {
            return false;
        };
        if chunk.len() > HEALTH_BODY_MAX_BYTES.saturating_sub(body.len()) {
            return false;
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .is_some_and(|value| {
            value.get("query").and_then(serde_json::Value::as_str) == Some(expected_query)
                && value
                    .get("results")
                    .is_some_and(serde_json::Value::is_array)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn sleeping_test_child() -> Child {
        use std::os::unix::process::CommandExt;
        let mut command = std::process::Command::new("sh");
        command.args(["-c", "sleep 30"]);
        command.process_group(0);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    #[cfg(windows)]
    fn sleeping_test_child() -> Child {
        use std::os::windows::process::CommandExt;
        let mut command = std::process::Command::new("cmd");
        command.args(["/C", "ping -n 31 127.0.0.1 >NUL"]);
        // CREATE_NO_WINDOW: under DETACHED_PROCESS this helper opened a real
        // console window on every test run, and killing the child orphaned it
        // on the developer's desktop.
        command.creation_flags(
            mermaid_model::utils::CREATE_NO_WINDOW | mermaid_model::utils::CREATE_NEW_PROCESS_GROUP,
        );
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    #[test]
    fn settings_enable_json_and_disable_limiter_and_valkey() {
        let s = settings_yml("test-secret");
        assert!(s.contains("use_default_settings:"));
        assert!(
            s.contains("- ahmia") && s.contains("- torch"),
            "dead onion engines must be removed:\n{s}"
        );
        assert!(s.contains("- json"), "JSON format must be enabled:\n{s}");
        assert!(s.contains("limiter: false"));
        assert!(s.contains("url: false"), "Valkey must be disabled:\n{s}");
        assert!(s.contains("secret_key:"));
        assert!(!s.contains("secret_key: \"\""), "secret must be non-empty");
    }

    #[test]
    fn reserved_port_remains_exclusive_until_release() {
        let reserved = reserve_port().unwrap();
        assert!(reserved.port() > 0);
        assert!(
            std::net::TcpListener::bind(("127.0.0.1", reserved.port())).is_err(),
            "another listener claimed the reserved managed-search port"
        );
        let port = reserved.port();
        drop(reserved);
        std::net::TcpListener::bind(("127.0.0.1", port))
            .expect("released reservation should be reusable");
    }

    #[test]
    fn cached_health_expires_after_the_ttl() {
        assert!(health_check_is_fresh(Some(Instant::now())));

        let expired_at = Instant::now()
            .checked_sub(HEALTH_CACHE_TTL + Duration::from_millis(1))
            .unwrap();
        assert!(!health_check_is_fresh(Some(expired_at)));
        assert!(!health_check_is_fresh(None));
    }

    #[test]
    fn python_bin_sits_under_the_runtime() {
        let py = python_bin(Path::new("/data/searxng/runtime"));
        assert!(py.starts_with("/data/searxng/runtime/python"), "{py:?}");
        assert!(py.ends_with(if cfg!(windows) {
            "python.exe"
        } else {
            "python3"
        }));
    }

    #[test]
    fn random_secret_is_hex_and_unique() {
        let a = random_secret().unwrap();
        let b = random_secret().unwrap();
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "each secret should be freshly random");
    }

    #[test]
    fn entropy_failure_blocks_secrets_and_settings_creation() {
        let error = random_secret_with(|_| Err(anyhow!("injected entropy failure"))).unwrap_err();
        assert!(error.to_string().contains("injected entropy failure"));

        let root = std::env::temp_dir().join(format!(
            "mermaid-searxng-entropy-test-{}-{}",
            std::process::id(),
            random_secret().unwrap()
        ));
        let error = write_settings_in_with(&root, || Err(anyhow!("server entropy failed")))
            .err()
            .expect("server entropy failure must abort settings creation");
        assert!(error.to_string().contains("server secret"), "{error}");

        let mut calls = 0;
        let error = write_settings_in_with(&root, || {
            calls += 1;
            if calls == 1 {
                Ok("server-secret".to_string())
            } else {
                Err(anyhow!("filename entropy failed"))
            }
        })
        .err()
        .expect("filename entropy failure must abort settings creation");
        assert!(error.to_string().contains("settings filename"), "{error}");
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
        std::fs::remove_dir(&root).unwrap();
    }

    #[tokio::test]
    async fn entropy_failure_blocks_health_challenge() {
        let client = health_client().unwrap();
        let error = probe_ready_with_secret(&client, "http://127.0.0.1:1", || {
            Err(anyhow!("health entropy failed"))
        })
        .await
        .unwrap_err();
        assert!(error.to_string().contains("health entropy failed"));
    }

    #[test]
    fn settings_files_are_unique_private_and_removed_with_their_owner() {
        let root = std::env::temp_dir().join(format!(
            "mermaid-searxng-settings-test-{}-{}",
            std::process::id(),
            random_secret().unwrap()
        ));
        let first = write_settings_in(&root).unwrap();
        let second = write_settings_in(&root).unwrap();
        assert_ne!(first.path, second.path);
        assert!(first.path.is_file());
        assert!(second.path.is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&first.path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let first_path = first.path.clone();
        let second_path = second.path.clone();
        drop(first);
        drop(second);
        assert!(!first_path.exists());
        assert!(!second_path.exists());
        std::fs::remove_dir(&root).unwrap();
    }

    #[tokio::test]
    async fn health_probe_requires_bounded_searxng_json() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        async fn serve(
            make_body: impl FnOnce(&str) -> Vec<u8> + Send + 'static,
        ) -> (String, tokio::task::JoinHandle<()>) {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0_u8; 1024];
                let read = stream.read(&mut request).await.unwrap();
                let request = String::from_utf8_lossy(&request[..read]);
                let query = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .and_then(|target| target.split_once('?').map(|(_, query)| query))
                    .and_then(|query| query.split('&').find_map(|pair| pair.strip_prefix("q=")))
                    .unwrap_or_default();
                let body = make_body(query);
                let headers = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(headers.as_bytes()).await.unwrap();
                stream.write_all(&body).await.unwrap();
            });
            (format!("http://{address}"), server)
        }

        let (base, server) =
            serve(|query| format!(r#"{{"query":"{query}","results":[]}}"#).into_bytes()).await;
        assert!(probe_ready(&base).await.unwrap());
        server.await.unwrap();

        let (base, server) = serve(|_| br#"{"query":"wrong","results":[]}"#.to_vec()).await;
        assert!(
            !probe_ready(&base).await.unwrap(),
            "a response that did not echo the challenge was trusted"
        );
        server.await.unwrap();

        let (base, server) = serve(|_| b"{}".to_vec()).await;
        assert!(
            !probe_ready(&base).await.unwrap(),
            "arbitrary JSON was trusted"
        );
        server.await.unwrap();

        let (base, server) = serve(|_| vec![b' '; HEALTH_BODY_MAX_BYTES + 1]).await;
        assert!(
            !probe_ready(&base).await.unwrap(),
            "oversized health body was buffered"
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn cancelled_readiness_keeps_process_owned_for_cleanup() {
        let root = std::env::temp_dir().join(format!(
            "mermaid-searxng-cancel-test-{}-{}",
            std::process::id(),
            random_secret().unwrap()
        ));
        let settings = write_settings_in(&root).unwrap();
        let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = reserved.local_addr().unwrap();
        let mut running = Some(Running {
            child: sleeping_test_child(),
            base_url: format!("http://{address}"),
            runtime: root.clone(),
            ready: false,
            started_at: Instant::now(),
            last_health_check: None,
            _settings: settings,
        });

        let readiness = tokio::time::timeout(
            Duration::from_millis(25),
            wait_ready(running.as_mut().unwrap()),
        )
        .await;
        assert!(readiness.is_err(), "readiness should have been cancelled");
        assert!(
            running.is_some(),
            "cancellation discarded process ownership"
        );
        drop(reserved);

        terminate_owned(&mut running, mermaid_model::utils::Grace::Immediate)
            .await
            .unwrap();
        assert!(running.is_none());
        std::fs::remove_dir(&root).unwrap();
    }

    #[tokio::test]
    async fn readiness_rejects_a_dead_owned_child() {
        let root = std::env::temp_dir().join(format!(
            "mermaid-searxng-dead-test-{}-{}",
            std::process::id(),
            random_secret().unwrap()
        ));
        let settings = write_settings_in(&root).unwrap();
        let mut child = sleeping_test_child();
        mermaid_model::utils::terminate_tree(child.id(), mermaid_model::utils::Grace::Immediate)
            .await;
        child.wait().unwrap();
        let mut running = Running {
            child,
            base_url: "http://127.0.0.1:1".to_string(),
            runtime: root.clone(),
            ready: false,
            started_at: Instant::now(),
            last_health_check: None,
            _settings: settings,
        };

        let error = wait_ready(&mut running).await.unwrap_err();
        assert!(error.runtime_suspect, "dead child did not trigger repair");
        drop(running);
        std::fs::remove_dir(&root).unwrap();
    }

    /// Full path against the published bundle: download + sha256-verify + unpack,
    /// spawn Granian, serve the JSON API, then reap. Ignored by default — it
    /// downloads a ~65-80 MB bundle, writes the data dir, and spawns a real
    /// server. Run with:
    ///   `cargo test --lib managed_searxng_end_to_end -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn managed_searxng_end_to_end() {
        if let Err(reason) = managed_backend_viability() {
            assert!(
                reason.contains(std::env::consts::OS),
                "unsupported-platform reason omitted the OS: {reason}"
            );
            return;
        }
        let base = manager()
            .ensure_running()
            .await
            .expect("ensure_running should download the bundle and start Granian");
        assert!(base.starts_with("http://127.0.0.1:"), "base_url: {base}");

        let probe = format!("{base}/search?q=mermaid&format=json");
        let resp = reqwest::Client::new()
            .get(&probe)
            .send()
            .await
            .expect("search probe");
        assert!(resp.status().is_success(), "status: {}", resp.status());
        let body: serde_json::Value = resp.json().await.expect("json body");
        assert!(body.get("results").is_some(), "no results field in {body}");

        shutdown().await;
        // After shutdown the port must be released (the Granian tree was reaped).
        let after = reqwest::Client::new()
            .get(&probe)
            .timeout(std::time::Duration::from_secs(2))
            .send()
            .await;
        assert!(
            after.is_err() || !after.unwrap().status().is_success(),
            "server still answering after shutdown — reap failed"
        );
    }
}
