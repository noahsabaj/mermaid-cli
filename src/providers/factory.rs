//! Runtime provider construction.
//!
//! `ProviderFactory` turns `(Config, model_id)` into the right
//! `Arc<dyn ModelProvider>`. The effect runner holds one of these
//! and asks it to build a provider the first time a new model is
//! referenced; subsequent lookups hit the cache.

use std::sync::Arc;

use tokio::sync::Mutex;

use crate::app::Config;
use crate::models::config::BackendConfig;
use crate::models::{ModelError, Result, lookup_provider};
use crate::utils::{resolve_api_key, resolve_api_key_with_fallback};

const GEMINI_API_KEY_ENV: &str = "GOOGLE_API_KEY";
const GEMINI_LEGACY_API_KEY_ENV: &str = "GEMINI_API_KEY";

/// Resolve an API key or return a clear `ModelError` when the env
/// var isn't set. Takes `default_env` (the registry-default name)
/// and allows no override — the factory passes the already-resolved
/// env name.
fn require_key(provider: &str, env_var: &str) -> Result<String> {
    resolve_api_key(env_var, None).ok_or_else(|| {
        ModelError::Authentication(format!("{} requires env var {}", provider, env_var))
    })
}

fn require_key_with_fallback(
    provider: &str,
    env_var: &str,
    fallback_env_var: &str,
) -> Result<String> {
    resolve_api_key_with_fallback(env_var, fallback_env_var, None).ok_or_else(|| {
        ModelError::Authentication(format!(
            "{} requires env var {} (or legacy {})",
            provider, env_var, fallback_env_var
        ))
    })
}

use super::model::{
    AnthropicProvider, GeminiProvider, ModelProvider, OllamaProvider, OpenAICompatProvider,
};

/// A lazily-built, shared provider. `OnceCell` gives single-flight construction
/// (#84); the outer `Arc` lets `resolve` clone the cell out from under the cache
/// lock and initialize it without holding the lock across the build.
type ProviderCell = Arc<tokio::sync::OnceCell<Arc<dyn ModelProvider>>>;

/// Per-process provider cache. Providers are expensive to construct
/// (HTTP client, connection pool, capability lookup) so the effect
/// runner asks for them lazily and reuses across turns.
pub struct ProviderFactory {
    config: Arc<Config>,
    /// Per-key cache of built providers, keyed by normalized model id (#83). The
    /// `Mutex` is only held to get-or-insert a cell, never across the build.
    cache: Mutex<std::collections::HashMap<String, ProviderCell>>,
}

impl ProviderFactory {
    pub fn new(config: Config) -> Self {
        Self {
            config: Arc::new(config),
            cache: Mutex::new(std::collections::HashMap::new()),
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Resolve (or lazily construct) a provider for the given model
    /// ID. Hits the cache on the second and subsequent calls for the
    /// same ID.
    pub async fn resolve(&self, model_id: &str) -> Result<Arc<dyn ModelProvider>> {
        let key = normalize_cache_key(model_id);
        // Get-or-insert the per-key cell under a brief lock, then initialize it
        // exactly once outside the lock (#84). A failed build isn't cached, so a
        // transient error can be retried on the next call.
        let cell = {
            let mut cache = self.cache.lock().await;
            Arc::clone(
                cache
                    .entry(key)
                    .or_insert_with(|| Arc::new(tokio::sync::OnceCell::new())),
            )
        };
        let provider = cell
            .get_or_try_init(|| async {
                let p = build_provider(&self.config, model_id).await?;
                Ok::<Arc<dyn ModelProvider>, ModelError>(Arc::from(p))
            })
            .await?;
        Ok(Arc::clone(provider))
    }
}

/// Build a provider for the given `model_id`:
///   1. `ollama/<model>` → OllamaProvider.
///   2. `anthropic/<model>` → AnthropicProvider.
///   3. `gemini/<model>` → GeminiProvider.
///   4. Other builtin providers (openai, openrouter, groq, …) → OpenAICompatProvider.
///   5. User-defined `[providers.<name>]` → custom OpenAICompatProvider.
///   6. Bare model name → OllamaProvider.
async fn build_provider(config: &Config, model_id: &str) -> Result<Box<dyn ModelProvider>> {
    let (provider, model_name) = parse_model_id(model_id);
    let provider_lc = provider.to_lowercase();

    // 1. Ollama (and bare names). F11: pass Arc<Config> so the wrapper
    // can forward Ollama hardware options to the adapter.
    if provider_lc == "ollama" {
        let backend = ollama_backend_config(config);
        let p = OllamaProvider::with_app_config(
            model_name,
            Arc::new(backend),
            Arc::new(config.clone()),
        )
        .await?;
        return Ok(Box::new(p));
    }

    // 2. Anthropic — bespoke API shape.
    if provider_lc == "anthropic" {
        let user_cfg = config.providers.get("anthropic");
        let base_url = user_cfg
            .and_then(|c| c.base_url.clone())
            .unwrap_or_else(|| "https://api.anthropic.com/v1".to_string());
        validate_provider_base_url(&base_url)?;
        let api_key_env = user_cfg
            .and_then(|c| c.api_key_env.as_deref())
            .unwrap_or("ANTHROPIC_API_KEY");
        let api_key = require_key("anthropic", api_key_env)?;
        let p = AnthropicProvider::new(api_key, model_name.to_string(), base_url)?;
        return Ok(Box::new(p));
    }

    // 3. Gemini — GCP AI Studio shape.
    if provider_lc == "gemini" {
        let user_cfg = config.providers.get("gemini");
        let base_url = user_cfg
            .and_then(|c| c.base_url.clone())
            .unwrap_or_else(|| "https://generativelanguage.googleapis.com/v1beta".to_string());
        validate_provider_base_url(&base_url)?;
        let api_key = match user_cfg.and_then(|c| c.api_key_env.as_deref()) {
            Some(api_key_env) => require_key("gemini", api_key_env)?,
            None => {
                require_key_with_fallback("gemini", GEMINI_API_KEY_ENV, GEMINI_LEGACY_API_KEY_ENV)?
            },
        };
        let p = GeminiProvider::new(api_key, model_name.to_string(), base_url)?;
        return Ok(Box::new(p));
    }

    // 4 + 5. OpenAI-compatible registry or user-custom.
    if let Some(profile) = lookup_provider(&provider_lc) {
        let user_cfg = config.providers.get(&provider_lc);
        let base_url = user_cfg
            .and_then(|c| c.base_url.clone())
            .unwrap_or_else(|| profile.base_url.to_string());
        validate_provider_base_url(&base_url)?;
        let api_key_env = user_cfg
            .and_then(|c| c.api_key_env.as_deref())
            .unwrap_or(profile.api_key_env);
        let api_key = require_key(&provider_lc, api_key_env)?;
        let extra_headers = user_cfg
            .map(|c| c.extra_headers.clone())
            .unwrap_or_default();
        let p = OpenAICompatProvider::new(
            profile,
            base_url,
            api_key,
            model_name.to_string(),
            extra_headers,
        )?;
        return Ok(Box::new(p));
    }

    // User-custom: no registry entry, but the user has [providers.<name>]
    // in config with a declared `compat` field.
    if let Some(user_cfg) = config.providers.get(&provider_lc)
        && let Some(profile) = user_profile_to_static(&provider_lc, user_cfg)
    {
        let base_url = user_cfg.base_url.clone().ok_or_else(|| {
            ModelError::InvalidRequest(format!(
                "custom provider '{}' requires base_url in config",
                provider_lc
            ))
        })?;
        validate_provider_base_url(&base_url)?;
        let api_key_env = user_cfg.api_key_env.as_deref().ok_or_else(|| {
            ModelError::InvalidRequest(format!(
                "custom provider '{}' requires api_key_env in config",
                provider_lc
            ))
        })?;
        let api_key = require_key(&provider_lc, api_key_env)?;
        let p = OpenAICompatProvider::new(
            profile,
            base_url,
            api_key,
            model_name.to_string(),
            user_cfg.extra_headers.clone(),
        )?;
        return Ok(Box::new(p));
    }

    Err(ModelError::InvalidRequest(format!(
        "Unknown provider '{}' (model_id: {})",
        provider, model_id
    )))
}

/// Cache key for a model id: the provider segment lowercased (matching
/// `build_provider`'s own normalization) so `Anthropic/x` and `anthropic/x`
/// resolve to one cached provider instead of building two identical ones (#83).
fn normalize_cache_key(model_id: &str) -> String {
    let (provider, model) = parse_model_id(model_id);
    format!("{}/{}", provider.to_lowercase(), model)
}

/// Parse `provider/model` → `(provider, model)`. Bare strings are
/// Ollama by convention.
fn parse_model_id(model_id: &str) -> (String, &str) {
    match model_id.split_once('/') {
        Some((p, m)) => (p.to_string(), m),
        None => ("ollama".to_string(), model_id),
    }
}

pub(crate) fn ollama_backend_config(config: &Config) -> BackendConfig {
    BackendConfig {
        // Scheme-less: `normalize_url` in the adapter picks http (loopback/LAN)
        // vs https (public) by host class (#86).
        ollama_url: format!("{}:{}", config.ollama.host, config.ollama.port),
        max_idle_per_host: 10,
        timeout_secs: 10,
    }
}

/// Convert a user-defined `[providers.<name>]` entry into a `&'static
/// ProviderProfile`. We need `&'static` because `ProviderProfile`'s
/// lifetime is tied to the registry constants; we leak a tiny owned
/// copy so custom providers can participate without redesigning the
/// profile type. Leaked allocations are bounded by the number of
/// custom providers (typically 0-3).
fn user_profile_to_static(
    name: &str,
    user_cfg: &crate::app::UserProviderConfig,
) -> Option<&'static crate::models::ProviderProfile> {
    use crate::models::{ProviderProfile, ReasoningExtraction, ReasoningStrategy};

    let compat = user_cfg.compat.as_deref().unwrap_or("openai");
    let strategy = match compat {
        "openai" => ReasoningStrategy::None,
        "openai-effort" => ReasoningStrategy::Effort,
        "openrouter" => ReasoningStrategy::OpenRouterShape,
        _ => ReasoningStrategy::None,
    };

    let profile = Box::new(ProviderProfile {
        name: Box::leak(name.to_string().into_boxed_str()),
        base_url: Box::leak(
            user_cfg
                .base_url
                .clone()
                .unwrap_or_default()
                .into_boxed_str(),
        ),
        api_key_env: Box::leak(
            user_cfg
                .api_key_env
                .clone()
                .unwrap_or_default()
                .into_boxed_str(),
        ),
        extra_headers: &[],
        reasoning_strategy: strategy,
        reasoning_extraction: ReasoningExtraction::None,
        max_tokens_param: crate::models::MaxTokensParam::MaxTokens,
        disable_parallel_tool_calls_for: &[],
    });
    Some(Box::leak(profile))
}

/// Validate a provider `base_url` before it's handed an API key. Requires
/// http/https, and **requires https for any non-loopback, non-private host** —
/// a typo'd or hostile `http://` endpoint would otherwise receive the bearer
/// key in cleartext. Plain http stays allowed for loopback / RFC-1918 hosts so
/// local model servers (Ollama, vLLM) keep working.
fn validate_provider_base_url(url: &str) -> Result<()> {
    let parsed = reqwest::Url::parse(url).map_err(|e| {
        ModelError::InvalidRequest(format!("invalid provider base_url '{url}': {e}"))
    })?;
    match parsed.scheme() {
        "https" => Ok(()),
        // Plaintext http is only safe to LOOPBACK: a key sent over http to any
        // other host — including a LAN/private one — crosses the wire in
        // cleartext. Every caller here is a key-bearing provider; keyless local
        // Ollama uses a separate path that doesn't validate.
        "http"
            if crate::utils::classify_host(parsed.host_str().unwrap_or_default()).is_loopback() =>
        {
            Ok(())
        },
        "http" => Err(ModelError::InvalidRequest(format!(
            "provider base_url '{url}' uses http:// to a non-loopback host — refusing to send the \
             API key in cleartext. Use https, or http://localhost for a local server."
        ))),
        other => Err(ModelError::InvalidRequest(format!(
            "provider base_url '{url}' has unsupported scheme '{other}' (use http or https)"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_requires_https_for_remote_hosts() {
        // Remote http is refused (would leak the bearer key in cleartext).
        assert!(validate_provider_base_url("http://api.example.com/v1").is_err());
        assert!(validate_provider_base_url("ftp://example.com").is_err());
        // https anywhere and http to LOOPBACK are fine.
        assert!(validate_provider_base_url("https://api.example.com/v1").is_ok());
        assert!(validate_provider_base_url("http://localhost:11434/v1").is_ok());
        assert!(validate_provider_base_url("http://127.0.0.1:8000").is_ok());
        assert!(validate_provider_base_url("http://[::1]:8000").is_ok());
        // #26: http to a non-loopback host (even a private LAN one) is refused —
        // the API key would otherwise cross the wire in cleartext.
        assert!(validate_provider_base_url("http://192.168.1.5:8080").is_err());
        assert!(validate_provider_base_url("http://169.254.169.254").is_err());
    }
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn unique_env(prefix: &str) -> String {
        static N: AtomicUsize = AtomicUsize::new(0);
        format!(
            "{}_{}_{}",
            prefix,
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        )
    }

    #[test]
    fn parse_bare_name_defaults_to_ollama() {
        let (p, m) = parse_model_id("qwen3-coder:30b");
        assert_eq!(p, "ollama");
        assert_eq!(m, "qwen3-coder:30b");
    }

    #[test]
    fn parse_prefixed() {
        let (p, m) = parse_model_id("anthropic/claude-opus-4-7");
        assert_eq!(p, "anthropic");
        assert_eq!(m, "claude-opus-4-7");
    }

    #[test]
    fn gemini_key_resolution_accepts_legacy_fallback() {
        let primary = unique_env("MERMAID_FACTORY_GEMINI_PRIMARY");
        let legacy = unique_env("MERMAID_FACTORY_GEMINI_LEGACY");
        temp_env::with_vars(
            [(primary.as_str(), None), (legacy.as_str(), Some("legacy"))],
            || {
                let resolved = require_key_with_fallback("gemini", &primary, &legacy)
                    .expect("legacy fallback should resolve");
                assert_eq!(resolved, "legacy");
            },
        );
    }

    #[test]
    fn gemini_key_resolution_prefers_google_primary() {
        let primary = unique_env("MERMAID_FACTORY_GEMINI_PRIMARY2");
        let legacy = unique_env("MERMAID_FACTORY_GEMINI_LEGACY2");
        temp_env::with_vars(
            [
                (primary.as_str(), Some("google")),
                (legacy.as_str(), Some("legacy")),
            ],
            || {
                let resolved = require_key_with_fallback("gemini", &primary, &legacy)
                    .expect("primary should resolve");
                assert_eq!(resolved, "google");
            },
        );
    }

    #[tokio::test]
    async fn factory_reports_unknown_provider_clearly() {
        let cfg = Config::default();
        let f = ProviderFactory::new(cfg);
        match f.resolve("totally-made-up/model").await {
            Ok(_) => panic!("expected error"),
            Err(e) => {
                let msg = format!("{}", e);
                assert!(
                    msg.contains("totally-made-up") || msg.contains("Unknown provider"),
                    "error message: {}",
                    msg
                );
            },
        }
    }

    #[test]
    fn normalize_cache_key_lowercases_provider_only() {
        // #83: provider segment is lowercased; the model segment is preserved.
        assert_eq!(
            normalize_cache_key("Anthropic/Claude-X"),
            "anthropic/Claude-X"
        );
        assert_eq!(
            normalize_cache_key("anthropic/Claude-X"),
            "anthropic/Claude-X"
        );
        // Bare names default to the ollama provider.
        assert_eq!(normalize_cache_key("qwen3:30b"), "ollama/qwen3:30b");
    }

    #[tokio::test]
    async fn resolve_is_single_flight_and_cached() {
        // Ollama is keyless and builds no network connection, so this resolves
        // offline. Two concurrent resolves with different provider casing must
        // return the same cached instance (#83 normalization + #84 single-flight).
        let f = ProviderFactory::new(Config::default());
        let (a, b) = tokio::join!(
            f.resolve("ollama/test-model"),
            f.resolve("Ollama/test-model"),
        );
        let a = a.expect("resolve a");
        let b = b.expect("resolve b");
        assert!(
            Arc::ptr_eq(&a, &b),
            "expected one cached provider for casing variants + concurrent resolve"
        );
    }
}
