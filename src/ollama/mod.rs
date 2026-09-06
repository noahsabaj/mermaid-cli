/// Ollama integration module - Gateway
mod cloud_setup;
mod detector;
mod guide;
mod installer;
mod observe;
mod server;
mod store;

pub use cloud_setup::{
    get_cloud_api_key, is_cloud_configured, is_cloud_model, prompt_cloud_setup_if_needed,
    setup_cloud_interactive,
};
pub use detector::is_installed;
pub use guide::detect_and_guide;
pub use installer::{ensure_model, local_models};
pub use observe::{LocalModelListing, observe_models};
pub use server::{AutostartError, OllamaAutostart, ensure_running};

/// The session's Ollama backend configuration: the one place the pool and
/// timeout knobs live. Two other sites used to hand-roll the literal with
/// different numbers.
pub(crate) fn backend_config(
    config: &mermaid_domain::Config,
) -> mermaid_model::models::BackendConfig {
    mermaid_model::models::BackendConfig {
        // Scheme-less: `normalize_url` in the adapter picks http (loopback/LAN)
        // vs https (public) by host class (#86).
        ollama_url: config.ollama.base_url(),
        max_idle_per_host: 10,
        timeout_secs: 10,
        ollama_autostart: config.ollama.auto_start,
    }
}

/// [`backend_config`] for a short probe (model listing, presence checks): a
/// tighter timeout and a smaller pool, everything else identical.
pub(crate) fn probe_config(
    config: &mermaid_domain::Config,
) -> mermaid_model::models::BackendConfig {
    mermaid_model::models::BackendConfig {
        timeout_secs: 5,
        max_idle_per_host: 2,
        ..backend_config(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A drift guard, not a bug fix: the probe config is the session config
    /// with a shorter timeout and a smaller pool, and nothing else.
    #[test]
    fn probe_config_differs_from_session_config_only_in_timeout_and_pool() {
        let config = mermaid_domain::Config::default();
        let session = backend_config(&config);
        let probe = probe_config(&config);
        assert_eq!(probe.ollama_url, session.ollama_url);
        assert_eq!(probe.ollama_autostart, session.ollama_autostart);
        assert!(probe.timeout_secs < session.timeout_secs);
        assert!(probe.max_idle_per_host < session.max_idle_per_host);
    }
}
