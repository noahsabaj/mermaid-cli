//! Auto-managed local SearXNG for zero-config `web_search`.
//!
//! When `web_search` resolves to the managed backend — the default (`[web]
//! search_backend = "auto"`) whenever no Ollama key is set — the first search
//! lazily provisions a self-contained SearXNG bundle (a portable CPython + the
//! Granian server + the SearXNG app, downloaded and sha256-verified from the
//! `mermaid-searxng` releases; see [`bundle`]) and spawns Granian bound to
//! `127.0.0.1:<port>`. No container runtime — no Docker, no Podman, no VM. The
//! server is torn down when the top-level `EffectRunner` shuts down (i.e. mermaid
//! exits), the same reap path as MCP servers. Nothing persists between sessions
//! and the user configures nothing.

pub mod bundle;

use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use tokio::sync::Mutex;

/// A started SearXNG server: the Granian process (spawned as a process-group
/// leader, so `stop()` can reap it *and* its forked workers) and where to reach
/// it.
struct Running {
    child: Child,
    base_url: String,
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
        if let Some(r) = running.as_ref() {
            return Ok(r.base_url.clone());
        }
        // Ensure the portable CPython + Granian + SearXNG bundle is unpacked; on
        // an unsupported platform this errors with the fallback hint.
        let runtime = bundle::ensure_bundle().await?;
        let port = free_port()?;
        let settings = write_settings()?;
        let mut child = spawn_granian(&runtime, port, &settings)
            .map_err(|e| anyhow!("could not start the SearXNG server (Granian): {e}"))?;
        let base_url = format!("http://127.0.0.1:{port}");
        if let Err(e) = wait_ready(&base_url, &mut child).await {
            // Don't leave a half-booted server (and its workers) behind.
            crate::utils::terminate_tree(child.id(), crate::utils::Grace::Immediate).await;
            let _ = child.wait();
            return Err(anyhow!("SearXNG did not become ready: {e}"));
        }
        *running = Some(Running {
            child,
            base_url: base_url.clone(),
        });
        Ok(base_url)
    }

    async fn stop(&self) {
        let mut running = self.running.lock().await;
        if let Some(mut r) = running.take() {
            // A real OS process (Granian forks workers) needs a process-group
            // reap, not the container era's `rm -f` by name.
            crate::utils::terminate_tree(r.child.id(), crate::utils::Grace::Graceful).await;
            let _ = r.child.wait();
        }
    }
}

/// Grab a free localhost TCP port by binding `:0` and releasing it. The TOCTOU
/// window is smaller than the container era's: this same process binds the port
/// (via Granian) moments later.
fn free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .map_err(|e| anyhow!("could not find a free port for SearXNG: {e}"))?;
    Ok(listener.local_addr()?.port())
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
fn spawn_granian(runtime: &Path, port: u16, settings: &Path) -> std::io::Result<Child> {
    let python = python_bin(runtime);
    let port_str = port.to_string();
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
        cmd.creation_flags(crate::utils::DETACHED_PROCESS | crate::utils::CREATE_NEW_PROCESS_GROUP);
    }
    cmd.spawn()
}

/// Write the SearXNG settings that enable the JSON API and disable the bot
/// limiter and the (optional) Valkey cache — all safe for a private localhost
/// instance — returning the `settings.yml` path to pass as `SEARXNG_SETTINGS_PATH`.
/// Regenerated each launch so a settings-schema change always propagates (the
/// container era wrote it once, which stranded stale files).
fn write_settings() -> Result<PathBuf> {
    let dir = crate::app::get_config_dir()?.join("searxng");
    std::fs::create_dir_all(&dir)?;
    let settings = dir.join("settings.yml");
    std::fs::write(&settings, settings_yml())?;
    Ok(settings)
}

fn settings_yml() -> String {
    format!(
        "use_default_settings: true\n\
         server:\n\
        \x20 secret_key: \"{}\"\n\
        \x20 limiter: false\n\
         search:\n\
        \x20 formats:\n\
        \x20   - html\n\
        \x20   - json\n\
         valkey:\n\
        \x20 url: false\n",
        random_secret()
    )
}

fn random_secret() -> String {
    let mut bytes = [0u8; 16];
    if getrandom::fill(&mut bytes).is_err() {
        return "mermaid-searxng-local".to_string();
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Poll the JSON search endpoint until it answers, the server dies, or the
/// cold-start budget passes. Granian import + engine load takes a few seconds on
/// the first request (there is no image pull to wait on any more).
async fn wait_ready(base_url: &str, child: &mut Child) -> Result<()> {
    let client = reqwest::Client::new();
    let probe = format!("{base_url}/search?q=ping&format=json");
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Ok(resp) = client
            .get(&probe)
            .timeout(Duration::from_secs(3))
            .send()
            .await
            && resp.status().is_success()
        {
            return Ok(());
        }
        // A dead child will never become healthy — surface its exit instead of
        // waiting out the full budget (and reap it so no zombie lingers).
        if let Ok(Some(status)) = child.try_wait() {
            return Err(anyhow!("the Granian server exited immediately ({status})"));
        }
        if Instant::now() >= deadline {
            return Err(anyhow!("no response from {base_url} after startup"));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_enable_json_and_disable_limiter_and_valkey() {
        let s = settings_yml();
        assert!(s.contains("use_default_settings: true"));
        assert!(s.contains("- json"), "JSON format must be enabled:\n{s}");
        assert!(s.contains("limiter: false"));
        assert!(s.contains("url: false"), "Valkey must be disabled:\n{s}");
        assert!(s.contains("secret_key:"));
        assert!(!s.contains("secret_key: \"\""), "secret must be non-empty");
    }

    #[test]
    fn free_port_is_nonzero() {
        assert!(free_port().unwrap() > 0);
    }

    #[test]
    fn python_bin_sits_under_the_runtime() {
        let py = python_bin(Path::new("/data/searxng/runtime"));
        assert!(py.starts_with("/data/searxng/runtime/python"), "{py:?}");
        assert!(py.ends_with(if cfg!(windows) { "python.exe" } else { "python3" }));
    }

    #[test]
    fn random_secret_is_hex_and_unique() {
        let a = random_secret();
        let b = random_secret();
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "each secret should be freshly random");
    }
}
