//! Local Ollama server lifecycle — detect a dead loopback server and start it.
//!
//! The product rule: the user should never have to leave mermaid to run
//! `ollama serve`. When a request to a *local* Ollama URL is refused, we
//! locate the binary, start `ollama serve` detached (it outlives mermaid and
//! ignores the TUI's Ctrl+C), wait for the URL to become healthy, and let the
//! caller retry. Remote URLs are never touched — you can't start a server on
//! someone else's machine.
//!
//! Only *intent* paths auto-start (chat, model listing the user asked for,
//! startup preflight). Diagnostics (`mermaid status` / `doctor`) observe
//! without healing — see the `autostart` flag their `BackendConfig`s set.
//! Runtime kill-switch: `MERMAID_OLLAMA_AUTOSTART=0` disables autostart
//! process-wide (containers/CI where spawning a GPU server is unwanted);
//! unit-test builds are hard-disabled so no test can ever spawn a real
//! server through a default-config adapter.
//!
//! Concurrency: attempts are serialized process-wide behind a tokio `Mutex`,
//! and a failed attempt is remembered for a short cooldown so concurrent
//! callers (chat + model list on a cold boot) can't spawn-storm. A marker is
//! armed at spawn time too, so a caller cancelled mid-wait (Esc during a
//! turn) can't let the next caller double-spawn against a still-booting
//! child. Holding the lock across awaits is deliberate; a cancelled caller
//! drops its future, releases the lock, and leaves the spawned server
//! running — the server is a system resource, not turn-scoped work.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use mermaid_model::utils::classify_host;

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
    /// Autostart is switched off for this process (`MERMAID_OLLAMA_AUTOSTART=0`
    /// or a unit-test build). Same pass-through contract as `NotLocal`.
    Disabled,
    /// No `ollama` binary on PATH or in the platform's default install
    /// locations.
    NotInstalled,
    /// A start was attempted (or recently attempted) but the URL never became
    /// reachable; carries the specific failure.
    Unhealthy(String),
}

/// The [`mermaid_model::models::adapters::ollama::LocalServerRecovery`] the model layer is handed when the user's config
/// allows autostart.
///
/// This is the whole inversion: `ensure_running` — process discovery, spawning,
/// health-polling — lives here, in the module that owns the Ollama process, and
/// the wire adapter receives it as a capability instead of reaching up for it.
/// An adapter constructed without one cannot start anything, which is what makes
/// the enumeration verbs (`list`, `status`, `doctor`, `/model`) read-only by
/// construction rather than by a `bool` they remember to pass.
pub struct OllamaAutostart;

#[async_trait::async_trait]
impl mermaid_model::models::adapters::ollama::LocalServerRecovery for OllamaAutostart {
    async fn ensure_running(
        &self,
        base_url: &str,
        notify: Option<&(dyn for<'a> Fn(&'a str) + Sync)>,
    ) -> std::result::Result<(), Option<String>> {
        // `hint()` already encodes "nothing useful to say" as `None` for the
        // pass-through cases (NotLocal / Disabled), so the mapping is total.
        ensure_running(base_url, notify).await.map_err(|e| e.hint())
    }
}

impl AutostartError {
    /// Human hint to append to the caller's connection error, or `None` when
    /// the error should pass through untouched (`NotLocal` / `Disabled`).
    pub fn hint(&self) -> Option<String> {
        match self {
            AutostartError::NotLocal | AutostartError::Disabled => None,
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

/// Process-wide single-flight + cooldown. Deliberately NOT keyed by URL:
/// every Ollama adapter in this process derives its URL from the single
/// `config.ollama.host:port`, so there is exactly one authority to guard. If
/// mermaid ever grows multi-endpoint Ollama support, key this by authority.
static STATE: std::sync::LazyLock<tokio::sync::Mutex<AttemptState>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(AttemptState { last_failure: None }));

/// Runtime kill-switch (see module docs). `cfg!(test)` hard-disables in unit
/// tests so a default-config adapter (`ollama_autostart: true` pointing at
/// localhost) can never start a real server on a contributor machine.
fn autostart_disabled() -> bool {
    cfg!(test) || std::env::var_os("MERMAID_OLLAMA_AUTOSTART").is_some_and(|v| v == "0")
}

/// The single user-visible line surfaced at the moment a start is actually
/// attempted. Owned here (next to the spawn) so every trigger path — TUI
/// chat, headless `mermaid run`, CLI model list — shows identical wording,
/// and so it can say the part users can't otherwise discover: the server is
/// detached and deliberately outlives mermaid.
pub const STARTING_NOTICE: &str =
    "Starting the local Ollama server (it stays running after mermaid exits)…";

/// Make sure a *local* Ollama server is listening at `base_url`, starting
/// `ollama serve` if needed. `Ok(())` means the URL answered a health probe
/// (whether it was already up or we just started it) and a retry is
/// worthwhile.
///
/// `notify` is invoked with [`STARTING_NOTICE`] exactly once, at the moment
/// a spawn is committed to — never for `NotLocal`/`Disabled`, never when the
/// probe finds the server already healthy, never when the binary is missing.
/// Callers route it to their user-visible surface (stream status line,
/// stderr); the up-to-15s wait behind a generic spinner, the invisible
/// detached process, and file-only tracing are otherwise all silent.
pub async fn ensure_running(
    base_url: &str,
    notify: Option<&(dyn for<'a> Fn(&'a str) + Sync)>,
) -> Result<(), AutostartError> {
    let authority = authority_of(base_url).to_string();
    if !classify_host(host_of(&authority)).is_loopback() {
        return Err(AutostartError::NotLocal);
    }
    if autostart_disabled() {
        return Err(AutostartError::Disabled);
    }
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(1))
        .build()
    else {
        return Err(AutostartError::Unhealthy(
            "could not build a health-probe HTTP client".to_string(),
        ));
    };

    let mut state = STATE.lock().await;
    // Probe FIRST, cooldown second: another caller may have revived the
    // server while we waited for the lock, and a server the user started by
    // hand must be picked up instantly even inside the cooldown window.
    if healthy(&client, base_url).await {
        state.last_failure = None;
        return Ok(());
    }
    if let Some((at, err)) = &state.last_failure
        && at.elapsed() < COOLDOWN
    {
        return Err(err.clone());
    }

    let outcome = start_and_wait(&mut state, &client, base_url, &authority, notify).await;
    state.last_failure = match &outcome {
        Ok(()) => None,
        Err(e) => Some((Instant::now(), e.clone())),
    };
    outcome
}

/// Locate the binary, spawn `ollama serve` detached, and poll `base_url`
/// until it answers or the deadline passes.
async fn start_and_wait(
    state: &mut AttemptState,
    client: &reqwest::Client,
    base_url: &str,
    authority: &str,
    notify: Option<&(dyn for<'a> Fn(&'a str) + Sync)>,
) -> Result<(), AutostartError> {
    let Some(binary) = find_binary() else {
        return Err(AutostartError::NotInstalled);
    };
    // The spawn is committed — this is the one moment the user hears about
    // it (tracing below lands in the log file, invisible in normal use).
    if let Some(notify) = notify {
        notify(STARTING_NOTICE);
    }
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
    // Arm the cooldown NOW, not only on failure: if our caller is cancelled
    // mid-wait (future dropped, lock released), the next caller must see a
    // recent attempt and wait out the boot instead of double-spawning a
    // second `serve` that just loses the port bind. The health-probe-first
    // order above still picks the booted server up instantly.
    state.last_failure = Some((
        Instant::now(),
        AutostartError::Unhealthy(
            "`ollama serve` was started moments ago and may still be coming up — retry shortly"
                .to_string(),
        ),
    ));

    let deadline = Instant::now() + STARTUP_DEADLINE;
    loop {
        if healthy(client, base_url).await {
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

/// One cheap liveness probe: `GET /api/version` with the client's short
/// timeout.
async fn healthy(client: &reqwest::Client, base_url: &str) -> bool {
    let url = format!("{}/api/version", base_url);
    matches!(client.get(&url).send().await, Ok(r) if r.status().is_success())
}

/// Spawn `ollama serve` detached: null stdio, its own process group (so the
/// TUI's Ctrl+C doesn't kill it), no console on Windows. The server
/// deliberately outlives mermaid — it's a shared system service, and killing
/// it on exit would break other Ollama clients.
fn spawn_serve(binary: &std::path::Path, authority: &str) -> std::io::Result<std::process::Child> {
    let mut cmd = std::process::Command::new(binary);
    cmd.arg("serve")
        // Bind exactly where mermaid expects the server. An inherited
        // OLLAMA_HOST pointing somewhere else (e.g. 0.0.0.0 for LAN
        // exposure) would start a server we then can't reach at `base_url`;
        // users who want a custom bind manage the server themselves and can
        // set `auto_start = false`.
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
        // CREATE_NO_WINDOW, never DETACHED_PROCESS: the latter leaves a
        // visible console window on Windows 11 (see utils::proc).
        cmd.creation_flags(
            mermaid_model::utils::CREATE_NO_WINDOW | mermaid_model::utils::CREATE_NEW_PROCESS_GROUP,
        );
    }
    cmd.spawn()
}

/// `ollama` from PATH, falling back to the platform installer's default
/// locations (PATH edits don't reach already-running shells, and the macOS
/// app bundle never touches PATH). Also the definition of "installed" used
/// by `detector::is_installed`, so the startup preflight and the autostart
/// can never disagree about whether Ollama exists.
pub(crate) fn find_binary() -> Option<PathBuf> {
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
/// Note: whether `ollama serve` itself accepts a bracketed IPv6 OLLAMA_HOST
/// is unverified — IPv6-loopback autostart is best-effort.
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
        // Checked BEFORE the test-build kill-switch, so this asserts the real
        // production gate order.
        for url in [
            "https://ollama.example.com",
            "http://192.168.1.50:11434",
            "http://10.0.0.7:11434",
        ] {
            match ensure_running(url, None).await {
                Err(AutostartError::NotLocal) => {},
                other => panic!("{url} must be NotLocal, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn test_builds_never_spawn_even_for_loopback() {
        // The cfg!(test) hard-off: a loopback URL in a unit-test build stops
        // at Disabled before the lock/probe/spawn machinery. This is what
        // makes default-config adapters (autostart=true, localhost:11434)
        // safe in every present and future test on machines with Ollama
        // installed.
        match ensure_running("http://127.0.0.1:11434", None).await {
            Err(AutostartError::Disabled) => {},
            other => panic!("expected Disabled in test builds, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn notice_fires_only_when_a_spawn_is_committed() {
        // The gate paths that return before a spawn (NotLocal, Disabled)
        // must NOT invoke `notify` — the notice's contract is "a start is
        // actually happening", so a remote URL or a killed switch stays
        // silent and no false "Starting…" line ever reaches the user.
        use std::sync::atomic::{AtomicBool, Ordering};
        let called = AtomicBool::new(false);
        let notify = |_: &str| called.store(true, Ordering::SeqCst);
        let _ = ensure_running("https://ollama.example.com", Some(&notify)).await;
        let _ = ensure_running("http://127.0.0.1:11434", Some(&notify)).await;
        assert!(
            !called.load(Ordering::SeqCst),
            "notify must not fire on NotLocal/Disabled paths"
        );
    }

    #[test]
    fn hints_are_actionable_and_passthrough_variants_are_silent() {
        assert!(AutostartError::NotLocal.hint().is_none());
        assert!(AutostartError::Disabled.hint().is_none());
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
