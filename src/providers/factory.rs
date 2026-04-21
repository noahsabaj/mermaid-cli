//! Runtime provider construction.
//!
//! `ProviderFactory` turns `(Config, model_id)` into the right
//! `Arc<dyn ModelProvider>`. The effect runner holds one of these
//! and asks it to build a provider the first time a new model is
//! referenced; subsequent lookups hit the cache.
//!
//! This replaces the v0.6 `ModelFactory` for the v0.7 path. The old
//! factory returned `Box<dyn Model>` (the legacy trait); we return
//! providers implementing the new `ModelProvider` trait defined in
//! `super::model`. The underlying adapter crates are the same — C3
//! and C4's wrappers delegate to them.

use std::sync::Arc;

use tokio::sync::Mutex;

use crate::app::Config;
use crate::models::config::BackendConfig;
use crate::models::{ModelError, Result, lookup_provider};
use crate::utils::resolve_api_key;

/// Resolve an API key or return a clear `ModelError` when the env
/// var isn't set. Takes `default_env` (the registry-default name)
/// and allows no override — the factory passes the already-resolved
/// env name.
fn require_key(provider: &str, env_var: &str) -> Result<String> {
    resolve_api_key(env_var, None).ok_or_else(|| {
        ModelError::Authentication(format!("{} requires env var {}", provider, env_var))
    })
}

use super::model::{
    AnthropicProvider, GeminiProvider, ModelProvider, OllamaProvider, OpenAICompatProvider,
};

/// Per-process provider cache. Providers are expensive to construct
/// (HTTP client, connection pool, capability lookup) so the effect
/// runner asks for them lazily and reuses across turns.
pub struct ProviderFactory {
    config: Arc<Config>,
    cache: Mutex<std::collections::HashMap<String, Arc<dyn ModelProvider>>>,
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
        {
            let cache = self.cache.lock().await;
            if let Some(p) = cache.get(model_id) {
                return Ok(Arc::clone(p));
            }
        }

        let provider = build_provider(&self.config, model_id).await?;
        let arc: Arc<dyn ModelProvider> = Arc::from(provider);

        let mut cache = self.cache.lock().await;
        cache.insert(model_id.to_string(), Arc::clone(&arc));
        Ok(arc)
    }
}

/// Build a provider for the given `model_id`, applying the same
/// routing rules as the v0.6 `ModelFactory::create_model`:
///   1. `ollama/<model>` → OllamaProvider.
///   2. `anthropic/<model>` → AnthropicProvider.
///   3. `gemini/<model>` → GeminiProvider.
///   4. Other builtin providers (openai, openrouter, groq, …) → OpenAICompatProvider.
///   5. User-defined `[providers.<name>]` → custom OpenAICompatProvider.
///   6. Bare model name → OllamaProvider (legacy compat).
async fn build_provider(config: &Config, model_id: &str) -> Result<Box<dyn ModelProvider>> {
    let (provider, model_name) = parse_model_id(model_id);
    let provider_lc = provider.to_lowercase();

    // 1. Ollama (and bare names).
    if provider_lc == "ollama" {
        let backend = ollama_backend_config(config);
        let p = OllamaProvider::new(model_name, Arc::new(backend)).await?;
        return Ok(Box::new(p));
    }

    // 2. Anthropic — bespoke API shape.
    if provider_lc == "anthropic" {
        let user_cfg = config.providers.get("anthropic");
        let base_url = user_cfg
            .and_then(|c| c.base_url.clone())
            .unwrap_or_else(|| "https://api.anthropic.com/v1".to_string());
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
        let api_key_env = user_cfg
            .and_then(|c| c.api_key_env.as_deref())
            .unwrap_or("GEMINI_API_KEY");
        let api_key = require_key("gemini", api_key_env)?;
        let p = GeminiProvider::new(api_key, model_name.to_string(), base_url)?;
        return Ok(Box::new(p));
    }

    // 4 + 5. OpenAI-compatible registry or user-custom.
    if let Some(profile) = lookup_provider(&provider_lc) {
        let user_cfg = config.providers.get(&provider_lc);
        let base_url = user_cfg
            .and_then(|c| c.base_url.clone())
            .unwrap_or_else(|| profile.base_url.to_string());
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

/// Parse `provider/model` → `(provider, model)`. Bare strings are
/// Ollama by convention.
fn parse_model_id(model_id: &str) -> (String, &str) {
    match model_id.split_once('/') {
        Some((p, m)) => (p.to_string(), m),
        None => ("ollama".to_string(), model_id),
    }
}

fn ollama_backend_config(config: &Config) -> BackendConfig {
    BackendConfig {
        ollama_url: format!("http://{}:{}", config.ollama.host, config.ollama.port),
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
    });
    Some(Box::leak(profile))
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
