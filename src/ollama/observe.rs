//! What Ollama models exist, answered read-only.
//!
//! The one entry point every enumeration surface shares (`mermaid list`,
//! `/model`, `status`, `doctor`, and the startup default probe). Autostart is
//! hard-off on this path — observing must never mutate, so a server the user
//! deliberately stopped stays stopped — and the on-disk store
//! ([`super::store`]) fills the blind spot that rule used to create: a
//! stopped server no longer hides what is installed.

use std::sync::Arc;

use mermaid_domain::Config;

/// The installed-model answer, tagged with where it came from — the surfaces
/// phrase "running" and "will start on use" differently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalModelListing {
    /// The daemon answered `/api/tags`: authoritative, including whatever a
    /// custom server-side store location would serve.
    Live(Vec<String>),
    /// The daemon was unreachable; names come from the on-disk manifest
    /// store. Produced only for a loopback host with the Ollama binary
    /// installed — the combination in which "starts automatically on use"
    /// is actually true.
    FromDisk(Vec<String>),
    /// The daemon was unreachable and disk had no answer: a remote host, no
    /// binary to start, or no populated store to read.
    Unreachable,
}

impl LocalModelListing {
    /// The names regardless of source, when there are any to show.
    #[must_use]
    pub fn models(&self) -> Option<&[String]> {
        match self {
            Self::Live(models) | Self::FromDisk(models) => Some(models),
            Self::Unreachable => None,
        }
    }
}

/// Answer "what Ollama models exist" without mutating anything.
///
/// Asks the daemon first, and when it is unreachable falls back to the
/// manifest store on disk. The disk is consulted lazily — a running server
/// never pays for the walk — and only when [`host_is_loopback`] and the
/// binary is installed.
pub async fn observe_models(config: &Config) -> LocalModelListing {
    let live = live_models(config).await;
    combine(live, || {
        (host_is_loopback(config) && super::is_installed())
            .then(super::store::installed_models)
            .flatten()
    })
}

/// The pure decision: a live answer wins outright (including a truthful
/// "running with nothing pulled"), disk answers only when live failed and
/// the walk found something, and everything else is unreachable.
fn combine(
    live: Option<Vec<String>>,
    disk: impl FnOnce() -> Option<Vec<String>>,
) -> LocalModelListing {
    live.map_or_else(
        || match disk() {
            Some(models) if !models.is_empty() => LocalModelListing::FromDisk(models),
            _ => LocalModelListing::Unreachable,
        },
        LocalModelListing::Live,
    )
}

/// `/api/tags` with autostart hard-off. `None` when the server could not be
/// reached — distinct from `Some(vec![])`, a running server with nothing
/// pulled. No recovery hook is ever attached here, so this path *cannot*
/// start a server (see `LocalServerRecovery`) — that absence is the
/// read-only guarantee, not a flag someone remembers to pass.
async fn live_models(config: &Config) -> Option<Vec<String>> {
    use mermaid_model::models::adapters::ollama::OllamaAdapter;
    use mermaid_model::models::{BackendConfig, Model};
    let backend = BackendConfig {
        ollama_url: config.ollama.base_url(),
        timeout_secs: 5,
        max_idle_per_host: 2,
        ollama_autostart: false,
    };
    match OllamaAdapter::new("__list__", Arc::new(backend)).await {
        Ok(adapter) => adapter.list_models().await.ok(),
        Err(_) => None,
    }
}

/// Whether the configured Ollama host is this machine. The disk may only
/// answer for a server this machine would itself run — a remote Ollama's
/// store lives on the remote machine, and listing our own disk for it would
/// be confidently wrong. Same host classification the autostart gate uses.
fn host_is_loopback(config: &Config) -> bool {
    let authority = config.ollama.base_url();
    let host = super::server::host_of(super::server::authority_of(&authority));
    mermaid_model::utils::classify_host(host).is_loopback()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().copied().map(String::from).collect()
    }

    /// The precedence table, exhaustively: live wins even when empty (a
    /// running server with nothing pulled is a truthful empty, not a reason
    /// to consult disk), disk answers only a dead server with a non-empty
    /// walk, and the rest is unreachable.
    #[test]
    fn combine_prefers_live_then_nonempty_disk() {
        assert_eq!(
            combine(Some(names(&["a"])), || Some(names(&["b"]))),
            LocalModelListing::Live(names(&["a"]))
        );
        assert_eq!(
            combine(Some(Vec::new()), || Some(names(&["b"]))),
            LocalModelListing::Live(Vec::new())
        );
        assert_eq!(
            combine(None, || Some(names(&["b"]))),
            LocalModelListing::FromDisk(names(&["b"]))
        );
        assert_eq!(combine(None, || None), LocalModelListing::Unreachable);
        assert_eq!(
            combine(None, || Some(Vec::new())),
            LocalModelListing::Unreachable
        );
    }

    /// The disk gate: loopback hosts (with or without an explicit scheme in
    /// the configured host) may read the local store; anything remote —
    /// public or LAN — must not, because the store that answers for that
    /// server is on that machine.
    #[test]
    fn disk_fallback_is_loopback_only() {
        let mut config = Config::default();
        assert!(host_is_loopback(&config), "default localhost is loopback");
        config.ollama.host = "http://127.0.0.1".to_string();
        assert!(host_is_loopback(&config));
        config.ollama.host = "ollama.example.com".to_string();
        assert!(!host_is_loopback(&config));
        config.ollama.host = "http://192.168.1.50".to_string();
        assert!(!host_is_loopback(&config), "LAN is not this machine");
    }
}
