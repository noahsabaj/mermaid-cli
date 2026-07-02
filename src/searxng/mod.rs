//! Auto-managed local SearXNG for zero-config `web_search`.
//!
//! When `web_search` resolves to the managed backend — the default (`[web]
//! search_backend = "auto"`) whenever no Ollama key is set — the first search
//! lazily starts a SearXNG container via podman (or docker), waits until its
//! JSON API answers, and queries it. The container is torn down when the
//! top-level `EffectRunner` shuts down (i.e. mermaid exits), the same reap path
//! as MCP servers. Nothing persists between sessions and the user configures
//! nothing.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Result, anyhow};
use tokio::sync::Mutex;

/// Pinned SearXNG image, fully-qualified so podman doesn't prompt for a registry.
const IMAGE: &str = "docker.io/searxng/searxng:latest";
/// Fixed container name so a crashed prior run is cleanly replaced (`--replace`)
/// instead of leaking duplicates.
const CONTAINER_NAME: &str = "mermaid-searxng";

/// A started container: which runtime owns it and where to reach it.
struct Running {
    runtime: &'static str,
    base_url: String,
}

#[derive(Default)]
pub struct SearxngManager {
    /// `None` until the container is up. The `Mutex` serializes concurrent
    /// first-searches so exactly one container starts.
    running: Mutex<Option<Running>>,
}

static MANAGER: OnceLock<SearxngManager> = OnceLock::new();

/// Process-global manager handle (mirrors the MCP manager's `'static` global).
pub fn manager() -> &'static SearxngManager {
    MANAGER.get_or_init(SearxngManager::default)
}

/// Tear down the managed container if one was started. Called from the
/// top-level `EffectRunner::shutdown` on mermaid exit; no-op otherwise.
pub async fn shutdown() {
    if let Some(mgr) = MANAGER.get() {
        mgr.stop().await;
    }
}

impl SearxngManager {
    /// Ensure a SearXNG container is running; return its base URL. Starts one on
    /// the first call, then caches the URL for the rest of the process.
    pub async fn ensure_running(&self) -> Result<String> {
        let mut running = self.running.lock().await;
        if let Some(r) = running.as_ref() {
            return Ok(r.base_url.clone());
        }
        let runtime = detect_runtime().ok_or_else(|| {
            anyhow!(
                "web_search needs a container runtime (podman or docker) to auto-start SearXNG. \
                 Install one, set OLLAMA_API_KEY, or point `[web] search_backend`/`searxng_url` \
                 at your own instance."
            )
        })?;
        let port = free_port()?;
        let config_dir = write_settings()?;
        start_container(runtime, port, &config_dir).await?;
        let base_url = format!("http://127.0.0.1:{port}");
        if let Err(e) = wait_ready(&base_url).await {
            // Don't leave a half-booted container behind on failure.
            let _ = run(runtime, &["rm", "-f", CONTAINER_NAME]).await;
            return Err(anyhow!("SearXNG container did not become ready: {e}"));
        }
        *running = Some(Running {
            runtime,
            base_url: base_url.clone(),
        });
        Ok(base_url)
    }

    async fn stop(&self) {
        let mut running = self.running.lock().await;
        if let Some(r) = running.take() {
            let _ = run(r.runtime, &["rm", "-f", CONTAINER_NAME]).await;
        }
    }
}

/// Prefer podman (Fedora default, rootless) then docker.
fn detect_runtime() -> Option<&'static str> {
    if which::which("podman").is_ok() {
        Some("podman")
    } else if which::which("docker").is_ok() {
        Some("docker")
    } else {
        None
    }
}

/// Grab a free localhost TCP port by binding `:0` and releasing it. A small
/// TOCTOU window remains before the container binds it — acceptable for a
/// single-user local instance.
fn free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .map_err(|e| anyhow!("could not find a free port for SearXNG: {e}"))?;
    Ok(listener.local_addr()?.port())
}

/// Write (once) the SearXNG settings that enable the JSON API and disable the
/// bot limiter (safe for a private localhost instance), returning the directory
/// to mount at `/etc/searxng`.
fn write_settings() -> Result<PathBuf> {
    let dir = crate::app::get_config_dir()?.join("searxng");
    std::fs::create_dir_all(&dir)?;
    let settings = dir.join("settings.yml");
    if !settings.exists() {
        std::fs::write(&settings, settings_yml())?;
    }
    Ok(dir)
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
        \x20   - json\n",
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

async fn start_container(runtime: &str, port: u16, config_dir: &Path) -> Result<()> {
    // `:Z` relabels the mount for SELinux (Fedora + podman); harmless elsewhere.
    let mount = format!("{}:/etc/searxng:Z", config_dir.display());
    let publish = format!("127.0.0.1:{port}:8080");
    let out = tokio::time::timeout(
        Duration::from_secs(300), // first run pulls the image
        run(
            runtime,
            &[
                "run",
                "-d",
                "--replace",
                "--name",
                CONTAINER_NAME,
                "-p",
                &publish,
                "-v",
                &mount,
                IMAGE,
            ],
        ),
    )
    .await
    .map_err(|_| anyhow!("timed out starting the SearXNG container (image pull too slow?)"))??;
    if !out.status.success() {
        return Err(anyhow!(
            "failed to start SearXNG via {runtime}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// Poll the JSON search endpoint until it answers. Once the image is present,
/// container boot is only a few seconds.
async fn wait_ready(base_url: &str) -> Result<()> {
    let client = reqwest::Client::new();
    let probe = format!("{base_url}/search?q=ping&format=json");
    for _ in 0..30 {
        if let Ok(resp) = client
            .get(&probe)
            .timeout(Duration::from_secs(3))
            .send()
            .await
            && resp.status().is_success()
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(1500)).await;
    }
    Err(anyhow!("no response from {base_url} after startup"))
}

async fn run(runtime: &str, args: &[&str]) -> Result<std::process::Output> {
    tokio::process::Command::new(runtime)
        .args(args)
        .output()
        .await
        .map_err(|e| anyhow!("failed to run {runtime}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_enable_json_with_a_secret() {
        let s = settings_yml();
        assert!(s.contains("use_default_settings: true"));
        assert!(s.contains("- json"), "JSON format must be enabled:\n{s}");
        assert!(s.contains("limiter: false"));
        assert!(s.contains("secret_key:"));
        assert!(!s.contains("secret_key: \"\""), "secret must be non-empty");
    }

    #[test]
    fn free_port_is_nonzero() {
        assert!(free_port().unwrap() > 0);
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
