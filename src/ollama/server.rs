//! Local Ollama server lifecycle — detect a dead loopback server and start it.
//!
//! The product rule: the user should never have to leave mermaid to run
//! `ollama serve`. When a request to a *local* Ollama URL is refused, we
//! locate the binary, start `ollama serve` detached (it outlives mermaid and
//! ignores the TUI's Ctrl+C), wait for the URL to become healthy, and let the
//! caller retry. Remote URLs are never touched — you can't start a server on
//! someone else's machine.
//!
//! Concurrency: attempts are serialized process-wide behind a tokio `Mutex`,
//! and a failed attempt is remembered for a short cooldown so concurrent
//! callers (chat + model list on a cold boot) can't spawn-storm. Holding the
//! lock across awaits is deliberate; a cancelled caller (Esc during a turn)
//! drops its future, releases the lock, and leaves the spawned server running
//! — the server is a system resource, not turn-scoped work.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use crate::utils::classify_host;

/// How long a failed start attempt suppresses new attempts. Long enough that
/// the retry storm of a single turn (chat + probes) collapses into one
/// attempt, short enough that "I just installed Ollama, try again" works.
const COOLDOWN: Duration = Duration::from_secs(15);
/// How long a freshly spawned `ollama serve` gets to become reachable.
/// Cold start (GPU discovery included) is typically 1–5s.
const STARTUP_DEADLINE: Duration = Duration::from_secs(15);
const POLL_INTERVAL: Duration = Duration::from_millis(300);

/// Why autostart couldn't produce a healthy server.
#[derive(Debug, Clone)]
pub enum AutostartError {
    /// The URL isn't loopback — autostart doesn't apply. Callers should
    /// surface their original connection error untouched.
    NotLocal,
    /// No `ollama` binary on PATH or in the platform's default install
    /// locations.
    NotInstalled,
    /// A start was attempted (or recently attempted) but the URL never became
    /// reachable; carries the specific failure.
    Unhealthy(String),
}

impl AutostartError {
    /// Human hint to append to the caller's connection error, or `None` when
    /// the error should pass through untouched (`NotLocal`).
    pub fn hint(&self) -> Option<String> {
        match self {
            AutostartError::NotLocal => None,
            AutostartError::NotInstalled => Some(
                "Ollama doesn't appear to be installed (not on PATH or in the default \
                 install locations) — install it from https://ollama.com/download"
                    .to_string(),
            ),
            AutostartError::Unhealthy(detail) => Some(format!("auto-start failed: {detail}")),
        }
    }
}

/// Serialized attempt state: the last failed attempt and its error, kept for
/// [`COOLDOWN`] so repeated connection failures don't re-spawn in a loop.
struct AttemptState {
    last_failure: Option<(Instant, AutostartError)>,
}

static STATE: std::sync::LazyLock<tokio::sync::Mutex<AttemptState>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(AttemptState { last_failure: None }));

/// Make sure a *local* Ollama server is listening at `base_url`, starting
/// `ollama serve` if needed. `Ok(())` means the URL answered a health probe
/// (whether it was already up or we just started it) and a retry is
/// worthwhile.
pub async fn ensure_running(base_url: &str) -> Result<(), AutostartError> {
    let authority = authority_of(base_url).to_string();
    if !classify_host(host_of(&authority)).is_loopback() {
        return Err(AutostartError::NotLocal);
    }

    let mut state = STATE.lock().await;
    // Another caller may have revived it while we waited for the lock — and
    // this same probe catches the "server came back on its own" race.
    if healthy(base_url).await {
        state.last_failure = None;
        return Ok(());
    }
    if let Some((at, err)) = &state.last_failure
        && at.elapsed() < COOLDOWN
    {
        return Err(err.clone());
    }

    let outcome = start_and_wait(base_url, &authority).await;
    state.last_failure = match &outcome {
        Ok(()) => None,
        Err(e) => Some((Instant::now(), e.clone())),
    };
    outcome
}

/// Locate the binary, spawn `ollama serve` detached, and poll `base_url`
/// until it answers or the deadline passes.
async fn start_and_wait(base_url: &str, authority: &str) -> Result<(), AutostartError> {
    let Some(binary) = find_binary() else {
        return Err(AutostartError::NotInstalled);
    };
    tracing::info!(
        binary = %binary.display(),
        authority,
        "ollama is not running — starting `ollama serve`"
    );
    let mut child = spawn_serve(&binary, authority).map_err(|e| {
        AutostartError::Unhealthy(format!(
            "could not launch `{} serve`: {e}",
            binary.display()
        ))
    })?;

    let deadline = Instant::now() + STARTUP_DEADLINE;
    loop {
        if healthy(base_url).await {
            tracing::info!(%base_url, "ollama serve is up");
            return Ok(());
        }
        // A dead child means it will never become healthy — report the exit
        // instead of polling out the full deadline (also reaps the process,
        // so no zombie lingers on unix).
        if let Ok(Some(status)) = child.try_wait() {
            return Err(AutostartError::Unhealthy(format!(
                "`ollama serve` exited immediately ({status}) — is another server \
                 holding the port, or is OLLAMA_HOST misconfigured?"
            )));
        }
        if Instant::now() >= deadline {
            return Err(AutostartError::Unhealthy(format!(
                "started `ollama serve` but {base_url} was not reachable within {}s",
                STARTUP_DEADLINE.as_secs()
            )));
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// One cheap liveness probe: `GET /api/version` with a short timeout.
async fn healthy(base_url: &str) -> bool {
    let url = format!("{}/api/version", base_url);
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(1))
        .build()
    else {
        return false;
    };
    matches!(client.get(&url).send().await, Ok(r) if r.status().is_success())
}

/// Spawn `ollama serve` detached: null stdio, its own process group (so the
/// TUI's Ctrl+C doesn't kill it), no console window on Windows. The server
/// deliberately outlives mermaid — it's a shared system service, and killing
/// it on exit would break other Ollama clients.
fn spawn_serve(binary: &std::path::Path, authority: &str) -> std::io::Result<std::process::Child> {
    let mut cmd = std::process::Command::new(binary);
    cmd.arg("serve")
        // Bind exactly where mermaid expects the server. An inherited
        // OLLAMA_HOST pointing somewhere else would start a server we then
        // can't reach at `base_url`.
        .env("OLLAMA_HOST", authority)
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
        // Same pair the exec tool's background launcher uses: no inherited
        // console, not in mermaid's Ctrl+C process group.
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    cmd.spawn()
}

/// `ollama` from PATH, falling back to the platform installer's default
/// locations (PATH edits don't reach already-running shells, and the macOS
/// app bundle never touches PATH).
fn find_binary() -> Option<PathBuf> {
    if let Ok(path) = which::which("ollama") {
        return Some(path);
    }
    known_install_paths().into_iter().find(|p| p.is_file())
}

#[cfg(target_os = "windows")]
fn known_install_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(base) = std::env::var_os("LOCALAPPDATA") {
        paths.push(
            PathBuf::from(base)
                .join("Programs")
                .join("Ollama")
                .join("ollama.exe"),
        );
    }
    if let Some(base) = std::env::var_os("ProgramFiles") {
        paths.push(PathBuf::from(base).join("Ollama").join("ollama.exe"));
    }
    paths
}

#[cfg(target_os = "macos")]
fn known_install_paths() -> Vec<PathBuf> {
    vec![
        PathBuf::from("/opt/homebrew/bin/ollama"),
        PathBuf::from("/usr/local/bin/ollama"),
        PathBuf::from("/Applications/Ollama.app/Contents/Resources/ollama"),
    ]
}

#[cfg(all(unix, not(target_os = "macos")))]
fn known_install_paths() -> Vec<PathBuf> {
    vec![
        PathBuf::from("/usr/local/bin/ollama"),
        PathBuf::from("/usr/bin/ollama"),
    ]
}

/// `http://localhost:11434` → `localhost:11434` (scheme and any path/query
/// stripped). The adapter's `normalize_url` guarantees a scheme is present,
/// but parse defensively.
fn authority_of(base_url: &str) -> &str {
    let rest = base_url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(base_url);
    rest.split(['/', '?', '#']).next().unwrap_or(rest)
}

/// Host part of an authority: `localhost:11434` → `localhost`,
/// `[::1]:11434` → `[::1]` (brackets kept; `classify_host` strips them).
fn host_of(authority: &str) -> &str {
    if let Some(end) = authority.rfind(']') {
        return &authority[..=end];
    }
    authority.split(':').next().unwrap_or(authority)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authority_strips_scheme_and_path() {
        assert_eq!(authority_of("http://localhost:11434"), "localhost:11434");
        assert_eq!(authority_of("http://127.0.0.1:11434/v1"), "127.0.0.1:11434");
        assert_eq!(
            authority_of("https://ollama.example.com/api?x=1"),
            "ollama.example.com"
        );
        assert_eq!(authority_of("localhost:11434"), "localhost:11434");
    }

    #[test]
    fn host_extracts_from_authority() {
        assert_eq!(host_of("localhost:11434"), "localhost");
        assert_eq!(host_of("127.0.0.1:8080"), "127.0.0.1");
        assert_eq!(host_of("[::1]:11434"), "[::1]");
        assert_eq!(host_of("localhost"), "localhost");
    }

    #[tokio::test]
    async fn remote_urls_are_never_started() {
        // The whole gate: autostart must refuse to act for a non-loopback
        // URL — no spawn, no health probe against third parties. (LAN/private
        // hosts count as remote too: mermaid can't start a server there.)
        for url in [
            "https://ollama.example.com",
            "http://192.168.1.50:11434",
            "http://10.0.0.7:11434",
        ] {
            match ensure_running(url).await {
                Err(AutostartError::NotLocal) => {},
                other => panic!("{url} must be NotLocal, got {other:?}"),
            }
        }
    }

    #[test]
    fn hints_are_actionable_and_notlocal_is_silent() {
        assert!(AutostartError::NotLocal.hint().is_none());
        let not_installed = AutostartError::NotInstalled.hint().expect("hint");
        assert!(not_installed.contains("https://ollama.com/download"));
        let unhealthy = AutostartError::Unhealthy("boom".into())
            .hint()
            .expect("hint");
        assert!(unhealthy.contains("boom"));
    }

    #[test]
    fn install_candidates_exist_per_platform() {
        // Shape check only — never spawns. Windows may legitimately return an
        // empty list if the env vars are unset; unix lists are static.
        let paths = known_install_paths();
        #[cfg(not(target_os = "windows"))]
        assert!(!paths.is_empty());
        for p in paths {
            assert!(p.to_string_lossy().to_lowercase().contains("ollama"));
        }
    }
}
