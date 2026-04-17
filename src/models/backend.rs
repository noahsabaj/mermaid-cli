//! Model factory - creates model instances from identifiers
//!
//! Parses model identifiers like "ollama/llama3" or "groq/qwen-qwq-32b"
//! and creates the appropriate adapter implementing the Model trait.
//! Also provides static convenience methods for common operations.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use super::adapters::openai_compat::OpenAICompatAdapter;
use super::config::BackendConfig;
use super::error::{BackendError, ModelError, Result};
use super::providers::{
    CompatStyle, ProviderProfile, ReasoningExtraction, ReasoningStrategy, lookup_provider,
};
use super::traits::Model;
use crate::app::Config;
use crate::utils::resolve_api_key;

/// Model factory - creates model instances.
///
/// Holds the resolved Ollama `BackendConfig` for the existing OllamaAdapter
/// path AND an optional `Arc<Config>` for the OpenAI-compatible dispatch
/// path (which needs `config.providers` to resolve user overrides + custom
/// providers). Old callers that go through `ModelFactory::new(BackendConfig)`
/// without supplying a `Config` still work — they just can't dispatch
/// non-Ollama providers and will get a clear error.
pub struct ModelFactory {
    config: Arc<BackendConfig>,
    user_config: Option<Arc<Config>>,
}

impl ModelFactory {
    /// Create a new model factory with explicit backend config.
    /// (Ollama-only path; non-Ollama dispatch will error with a clear
    /// message until the factory is given a `Config` via `from_config`.)
    pub fn new(config: BackendConfig) -> Self {
        Self {
            config: Arc::new(config),
            user_config: None,
        }
    }

    /// Create a factory from app::Config — preserves the user's provider
    /// configuration so OpenAI-compat dispatch can find overrides + custom
    /// providers later.
    pub fn from_config(config: &Config) -> Self {
        Self {
            config: Arc::new(Self::config_to_backend_config(config)),
            user_config: Some(Arc::new(config.clone())),
        }
    }

    /// Create a model from a full identifier (e.g., "ollama/llama3" or
    /// "groq/qwen-qwq-32b"). Provider name resolution order:
    ///   1. `"ollama"` → existing OllamaAdapter.
    ///   2. Built-in OpenAI-compat registry (`groq`, `openai`, etc.) →
    ///      OpenAICompatAdapter, with optional user overrides applied.
    ///   3. User-defined provider in `config.providers.<name>` → custom
    ///      OpenAICompatAdapter built from the user's `compat = "..."`
    ///      profile spec.
    ///   4. Otherwise: clear "unknown provider" error.
    pub async fn create_model(&self, model_id: &str) -> Result<Box<dyn Model>> {
        let (provider, model_name) = parse_model_id(model_id);
        let provider_lc = provider.to_lowercase();

        // Path 1: Ollama (existing behavior, unchanged).
        if provider_lc == "ollama" {
            use super::adapters::ollama::OllamaAdapter;
            let adapter = OllamaAdapter::new(model_name, self.config.clone()).await?;
            return Ok(Box::new(adapter));
        }

        // Paths 2 + 3 + 4 require a user config to look up provider overrides
        // and custom-provider definitions.
        let user_config = self.user_config.as_ref().ok_or_else(|| {
            ModelError::InvalidRequest(format!(
                "Provider '{}' requires app config; use ModelFactory::from_config",
                provider
            ))
        })?;

        // Path 2: Anthropic — bespoke Messages API (not OpenAI-compat).
        if provider_lc == "anthropic" {
            use super::adapters::anthropic::AnthropicAdapter;
            let user_cfg = user_config.providers.get("anthropic");
            let base_url = user_cfg
                .and_then(|c| c.base_url.clone())
                .unwrap_or_else(|| "https://api.anthropic.com/v1".to_string());
            let api_key = resolve_api_key(
                "ANTHROPIC_API_KEY",
                user_cfg.and_then(|c| c.api_key_env.as_deref()),
            )
            .ok_or_else(|| {
                ModelError::Authentication(
                    "ANTHROPIC_API_KEY not set (or set [providers.anthropic].api_key_env to a \
                     custom env var)"
                        .to_string(),
                )
            })?;
            let adapter = AnthropicAdapter::new(api_key, model_name.to_string(), base_url)?;
            return Ok(Box::new(adapter));
        }

        // Path 3: Gemini — bespoke generateContent API (not OpenAI-compat).
        if provider_lc == "gemini" {
            use super::adapters::gemini::GeminiAdapter;
            let user_cfg = user_config.providers.get("gemini");
            let base_url = user_cfg
                .and_then(|c| c.base_url.clone())
                .unwrap_or_else(|| "https://generativelanguage.googleapis.com/v1beta".to_string());
            let api_key = resolve_api_key(
                "GOOGLE_API_KEY",
                user_cfg.and_then(|c| c.api_key_env.as_deref()),
            )
            .ok_or_else(|| {
                ModelError::Authentication(
                    "GOOGLE_API_KEY not set (or set [providers.gemini].api_key_env to a custom \
                     env var)"
                        .to_string(),
                )
            })?;
            let adapter = GeminiAdapter::new(api_key, model_name.to_string(), base_url)?;
            return Ok(Box::new(adapter));
        }

        // Path 4: built-in OpenAI-compat registry.
        if let Some(profile) = lookup_provider(&provider_lc) {
            let user_cfg = user_config.providers.get(&provider_lc);
            let base_url = user_cfg
                .and_then(|c| c.base_url.clone())
                .unwrap_or_else(|| profile.base_url.to_string());
            let api_key = resolve_api_key(
                profile.api_key_env,
                user_cfg.and_then(|c| c.api_key_env.as_deref()),
            )
            .ok_or_else(|| {
                ModelError::Authentication(format!(
                    "{} API key not set (env: {})",
                    provider_lc, profile.api_key_env
                ))
            })?;
            let mut headers: HashMap<String, String> = profile
                .extra_headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            if let Some(c) = user_cfg {
                for (k, v) in &c.extra_headers {
                    headers.insert(k.clone(), v.clone());
                }
            }
            let adapter = OpenAICompatAdapter::new(
                profile,
                base_url,
                api_key,
                model_name.to_string(),
                headers,
            )?;
            return Ok(Box::new(adapter));
        }

        // Path 5: fully custom provider (must declare `compat = "..."` +
        // base_url + api_key_env).
        if let Some(user_cfg) = user_config.providers.get(&provider_lc) {
            let base_url = user_cfg.base_url.clone().ok_or_else(|| {
                ModelError::Config(super::error::ConfigError::MissingRequired(format!(
                    "providers.{}.base_url",
                    provider_lc
                )))
            })?;
            let api_key_env = user_cfg.api_key_env.as_deref().ok_or_else(|| {
                ModelError::Config(super::error::ConfigError::MissingRequired(format!(
                    "providers.{}.api_key_env",
                    provider_lc
                )))
            })?;
            let api_key = resolve_api_key(api_key_env, None).ok_or_else(|| {
                ModelError::Authentication(format!(
                    "{} API key not set (env: {})",
                    provider_lc, api_key_env
                ))
            })?;
            let compat_style: CompatStyle = match user_cfg.compat.as_deref() {
                Some(s) => {
                    serde_json::from_value::<CompatStyle>(serde_json::Value::String(s.to_string()))
                        .map_err(|_| {
                            ModelError::Config(super::error::ConfigError::InvalidValue {
                                field: format!("providers.{}.compat", provider_lc),
                                value: s.to_string(),
                                reason: "must be one of: openai, openai-effort, openrouter"
                                    .to_string(),
                            })
                        })?
                },
                None => CompatStyle::Openai,
            };
            // Synthesize a profile from the user's compat declaration.
            // Leak the strings to give them 'static lifetime — the
            // ProviderProfile lives for the rest of the process anyway,
            // and adapter creation happens infrequently.
            let profile = synthesize_custom_profile(&provider_lc, compat_style);
            let headers: HashMap<String, String> = user_cfg.extra_headers.clone();
            let adapter = OpenAICompatAdapter::new(
                profile,
                base_url,
                api_key,
                model_name.to_string(),
                headers,
            )?;
            return Ok(Box::new(adapter));
        }

        Err(ModelError::InvalidRequest(format!(
            "Unknown provider: '{}'. Built-in providers: ollama, anthropic, gemini, openai, \
             groq, openrouter, cerebras, deepinfra, together. Add custom providers to \
             ~/.config/mermaid/config.toml under [providers.<name>].",
            provider
        )))
    }

    // --- Static convenience API ---

    /// Create a model instance from a model identifier with optional app config.
    ///
    /// Format examples:
    /// - "ollama/qwen3-coder:30b" - Explicit Ollama provider
    /// - "qwen3-coder:30b" - Defaults to Ollama
    /// - "kimi-k2.5:cloud" - Ollama cloud model
    /// - "groq/qwen-qwq-32b" - Groq via OpenAICompatAdapter (requires GROQ_API_KEY)
    /// - "openai/gpt-5-mini" - OpenAI via OpenAICompatAdapter (requires OPENAI_API_KEY)
    pub async fn create(model_id: &str, config: Option<&Config>) -> Result<Box<dyn Model>> {
        let factory = match config {
            Some(c) => Self::from_config(c),
            None => Self::new(BackendConfig::default()),
        };
        factory.create_model(model_id).await
    }

    /// List all models from all available providers.
    pub async fn list_all_models(config: &Config) -> Result<Vec<String>> {
        let factory = Self::from_config(config);
        let providers = factory.available_providers_impl().await;

        let mut all_models = Vec::new();
        for provider in providers {
            if let Ok(models) = factory.list_models(&provider).await {
                for model_name in models {
                    all_models.push(format!("{}/{}", provider, model_name));
                }
            }
        }

        all_models.sort();
        Ok(all_models)
    }

    /// Get list of available providers (static convenience)
    pub async fn available_providers() -> Vec<String> {
        let factory = Self::new(BackendConfig::default());
        factory.available_providers_impl().await
    }

    /// List available providers using this factory's config (instance method)
    pub async fn available_providers_pub(&self) -> Vec<String> {
        self.available_providers_impl().await
    }

    // --- Instance methods ---

    /// List available providers with a fast single-shot health check.
    ///
    /// Ollama gets a `GET /api/tags` probe with a 2s timeout. OpenAI-compat
    /// providers configured by the user are listed if their API key
    /// resolves (no network probe — would slow `mermaid status` and many
    /// providers don't expose a public `/health`).
    async fn available_providers_impl(&self) -> Vec<String> {
        let mut providers = Vec::new();

        // Quick Ollama check: single GET with 2s timeout, no retries.
        let url = format!(
            "{}/api/tags",
            self.config.ollama_url.trim().trim_end_matches('/')
        );
        if let Ok(client) = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            && let Ok(resp) = client.get(&url).send().await
            && resp.status().is_success()
        {
            providers.push("ollama".to_string());
        }

        // Anthropic: listed if ANTHROPIC_API_KEY resolves (or
        // [providers.anthropic].api_key_env override). No network probe —
        // Anthropic has no public health endpoint and the call would slow
        // `mermaid status`.
        if let Some(user_config) = self.user_config.as_ref() {
            let user_cfg = user_config.providers.get("anthropic");
            if resolve_api_key(
                "ANTHROPIC_API_KEY",
                user_cfg.and_then(|c| c.api_key_env.as_deref()),
            )
            .is_some()
            {
                providers.push("anthropic".to_string());
            }
        }

        // Gemini: listed if GOOGLE_API_KEY resolves (or override). Same
        // no-network-probe rationale as Anthropic — keeps `mermaid status`
        // snappy.
        if let Some(user_config) = self.user_config.as_ref() {
            let user_cfg = user_config.providers.get("gemini");
            if resolve_api_key(
                "GOOGLE_API_KEY",
                user_cfg.and_then(|c| c.api_key_env.as_deref()),
            )
            .is_some()
            {
                providers.push("gemini".to_string());
            }
        }

        // OpenAI-compat providers: include any built-in or user-defined
        // provider whose API key resolves. No network probe.
        if let Some(user_config) = self.user_config.as_ref() {
            // Built-in providers configured via env var or [providers.<name>].
            for profile in super::providers::REGISTRY {
                let user_cfg = user_config.providers.get(profile.name);
                let api_key_present = resolve_api_key(
                    profile.api_key_env,
                    user_cfg.and_then(|c| c.api_key_env.as_deref()),
                )
                .is_some();
                if api_key_present {
                    providers.push(profile.name.to_string());
                }
            }
            // Custom providers (not in registry) — listed if they have
            // both base_url and api_key_env declared and the env resolves.
            for (name, cfg) in &user_config.providers {
                if lookup_provider(name).is_some() {
                    continue; // already handled above
                }
                if let (Some(_url), Some(env)) = (&cfg.base_url, cfg.api_key_env.as_deref())
                    && resolve_api_key(env, None).is_some()
                {
                    providers.push(name.clone());
                }
            }
        }

        providers
    }

    /// List all models from a provider without creating a model instance.
    pub async fn list_models(&self, provider: &str) -> Result<Vec<String>> {
        let lc = provider.to_lowercase();
        // Anthropic has no public model-listing endpoint. Return a curated
        // list of supported Claude models so `mermaid list` is useful.
        // Users can still pass any Claude model name — the adapter doesn't
        // validate against this list.
        if lc == "anthropic" {
            return Ok(vec![
                "claude-opus-4-7".to_string(),
                "claude-sonnet-4-6".to_string(),
                "claude-opus-4-6".to_string(),
                "claude-sonnet-4-5".to_string(),
                "claude-opus-4-5".to_string(),
                "claude-haiku-4-5".to_string(),
            ]);
        }
        // Gemini: same rationale as Anthropic — curated list keeps
        // `mermaid list` snappy. Adapter accepts any model name; this
        // surfaces the recommended set.
        //
        // As of April 2026, Gemini 3 is preview-only — the bare IDs
        // `gemini-3-pro`, `gemini-3-flash`, `gemini-3-flash-lite` are
        // NOT valid and the original `gemini-3-pro-preview` was
        // shut down 2026-03-09. Current preview IDs are listed below,
        // plus the `-latest` aliases which repoint as Google ships
        // new previews. Re-verify against ai.google.dev quarterly.
        if lc == "gemini" {
            return Ok(vec![
                "gemini-pro-latest".to_string(),
                "gemini-flash-latest".to_string(),
                "gemini-3.1-pro-preview".to_string(),
                "gemini-3-flash-preview".to_string(),
                "gemini-3.1-flash-lite-preview".to_string(),
                "gemini-2.5-pro".to_string(),
                "gemini-2.5-flash".to_string(),
                "gemini-2.5-flash-lite".to_string(),
            ]);
        }
        if lc == "ollama" {
            let url = format!(
                "{}/api/tags",
                self.config.ollama_url.trim().trim_end_matches('/')
            );
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .map_err(|e| {
                    ModelError::Backend(BackendError::ConnectionFailed {
                        backend: "ollama".to_string(),
                        url: url.clone(),
                        reason: e.to_string(),
                    })
                })?;
            let response = client.get(&url).send().await.map_err(|e| {
                ModelError::Backend(BackendError::ConnectionFailed {
                    backend: "ollama".to_string(),
                    url: url.clone(),
                    reason: e.to_string(),
                })
            })?;
            if !response.status().is_success() {
                return Err(ModelError::Backend(BackendError::HttpError {
                    status: response.status().as_u16(),
                    message: "Failed to list models".to_string(),
                }));
            }
            let tags: super::adapters::ollama::OllamaTagsResponse =
                response.json().await.map_err(|e| ModelError::ParseError {
                    message: format!("Failed to parse tags response: {}", e),
                    raw: None,
                })?;
            return Ok(tags.models.into_iter().map(|m| m.name).collect());
        }

        // OpenAI-compat providers: build the adapter and ask it. The
        // adapter handles the `/models` endpoint and the providers that
        // 404 it.
        //
        // Use a synthetic model name ("_") because list_models() doesn't
        // need a specific model — it just needs the URL + key + headers.
        let synthetic_model_id = format!("{}/_", lc);
        let adapter = self.create_model(&synthetic_model_id).await?;
        adapter.list_models().await
    }

    /// Convert app::Config to BackendConfig (Ollama-specific).
    fn config_to_backend_config(config: &Config) -> BackendConfig {
        let ollama_url = format!("http://{}:{}", config.ollama.host, config.ollama.port);
        BackendConfig {
            ollama_url,
            timeout_secs: 10,
            max_idle_per_host: 10,
        }
    }
}

/// Synthesize a `'static` `ProviderProfile` for a fully custom provider
/// the user declared in config.toml. We leak the name/url strings into
/// `'static` because the profile is reused for the life of the process
/// (model creation runs at most a few times per session). For
/// `extra_headers` we use the empty slice — user headers come through
/// the adapter's per-instance `extra_headers` HashMap, not the profile.
fn synthesize_custom_profile(name: &str, compat: CompatStyle) -> &'static ProviderProfile {
    // Box::leak gives us a 'static reference with no copy. Names are
    // small (typical: 5-15 bytes) and one-per-custom-provider, so the
    // leak is bounded by the number of custom providers in config.toml.
    let leaked_name: &'static str = Box::leak(name.to_string().into_boxed_str());
    let extraction = match compat {
        CompatStyle::Openai => ReasoningExtraction::None,
        CompatStyle::OpenaiEffort => ReasoningExtraction::None,
        CompatStyle::Openrouter => ReasoningExtraction::DeltaContentField("reasoning"),
    };
    let profile = ProviderProfile {
        name: leaked_name,
        // base_url is overridden at adapter-construction time from the
        // user_cfg, so the placeholder here is only for `name()`-style
        // diagnostics. Use the leaked-name to make it discoverable.
        base_url: "user-defined",
        api_key_env: "user-defined",
        extra_headers: &[],
        reasoning_strategy: match compat {
            CompatStyle::Openai => ReasoningStrategy::None,
            CompatStyle::OpenaiEffort => ReasoningStrategy::Effort,
            CompatStyle::Openrouter => ReasoningStrategy::OpenRouterShape,
        },
        reasoning_extraction: extraction,
    };
    Box::leak(Box::new(profile))
}

/// Parse a model identifier into provider and model name.
///
/// Formats:
/// - "ollama/llama3" -> ("ollama", "llama3")
/// - "llama3" -> ("ollama", "llama3")  // defaults to ollama
/// - "llama3:latest" -> ("ollama", "llama3:latest")  // ollama tag format
/// - "groq/qwen-qwq-32b" -> ("groq", "qwen-qwq-32b")
/// - "together/deepseek-ai/DeepSeek-R1" -> ("together", "deepseek-ai/DeepSeek-R1")
///   (Together model names contain `/`; we split on the FIRST `/` only.)
fn parse_model_id(model_id: &str) -> (&str, &str) {
    if let Some(idx) = model_id.find('/') {
        let provider = &model_id[..idx];
        let model = &model_id[idx + 1..];
        (provider, model)
    } else {
        ("ollama", model_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_model_id_with_provider() {
        let (provider, model) = parse_model_id("ollama/llama3");
        assert_eq!(provider, "ollama");
        assert_eq!(model, "llama3");
    }

    #[test]
    fn test_parse_model_id_bare_name() {
        let (provider, model) = parse_model_id("llama3");
        assert_eq!(provider, "ollama");
        assert_eq!(model, "llama3");
    }

    #[test]
    fn test_parse_model_id_with_tag() {
        let (provider, model) = parse_model_id("ollama/llama3:latest");
        assert_eq!(provider, "ollama");
        assert_eq!(model, "llama3:latest");
    }

    #[test]
    fn test_parse_model_id_bare_with_tag() {
        let (provider, model) = parse_model_id("llama3:7b");
        assert_eq!(provider, "ollama");
        assert_eq!(model, "llama3:7b");
    }

    /// Together / DeepInfra model names contain `/` (e.g.
    /// `deepseek-ai/DeepSeek-R1`). The split must keep everything after
    /// the FIRST slash as the model name so the dispatcher gets the
    /// provider correct and passes the full model path through.
    #[test]
    fn test_parse_model_id_keeps_slashes_in_model_name() {
        let (provider, model) = parse_model_id("together/deepseek-ai/DeepSeek-R1");
        assert_eq!(provider, "together");
        assert_eq!(model, "deepseek-ai/DeepSeek-R1");
    }

    #[test]
    fn test_model_spec_parsing() {
        let specs = vec![
            ("ollama/tinyllama", Some("ollama"), "tinyllama"),
            ("qwen3-coder:30b", None, "qwen3-coder:30b"),
            ("kimi-k2.5:cloud", None, "kimi-k2.5:cloud"),
        ];
        for (spec, expected_provider, expected_model) in specs {
            let parts: Vec<&str> = spec.split('/').collect();
            if parts.len() == 2 {
                assert_eq!(Some(parts[0]), expected_provider);
                assert_eq!(parts[1], expected_model);
            } else {
                assert_eq!(None, expected_provider);
                assert_eq!(spec, expected_model);
            }
        }
    }

    #[test]
    fn test_provider_extraction() {
        fn extract_provider(spec: &str) -> Option<&str> {
            spec.split('/').next().filter(|_| spec.contains('/'))
        }
        assert_eq!(extract_provider("ollama/tinyllama"), Some("ollama"));
        assert_eq!(extract_provider("qwen3-coder:30b"), None);
    }

    #[tokio::test]
    async fn unknown_provider_returns_clear_error() {
        let cfg = Config::default();
        let factory = ModelFactory::from_config(&cfg);
        // `Box<dyn Model>` doesn't implement Debug, so we can't use
        // `.unwrap_err()`; pattern-match the result instead.
        match factory.create_model("nonexistent/foo").await {
            Ok(_) => panic!("expected unknown-provider error"),
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("Unknown provider"),
                    "expected 'Unknown provider' in: {}",
                    msg
                );
                assert!(
                    msg.contains("nonexistent"),
                    "error should name the bad provider; got: {}",
                    msg
                );
            },
        }
    }

    #[tokio::test]
    async fn missing_api_key_returns_authentication_error() {
        // `temp_env::async_with_vars` scopes the unset to this test only
        // and restores the prior value afterward — no `unsafe` needed,
        // and safe under `--test-threads > 1`.
        temp_env::async_with_vars([("GROQ_API_KEY", None::<&str>)], async {
            let cfg = Config::default();
            let factory = ModelFactory::from_config(&cfg);
            match factory.create_model("groq/qwen-qwq-32b").await {
                Ok(_) => panic!("expected auth error"),
                Err(e) => {
                    let msg = e.to_string();
                    assert!(
                        msg.contains("API key") || msg.contains("Authentication"),
                        "expected auth error, got: {}",
                        msg
                    );
                    assert!(
                        msg.contains("GROQ_API_KEY"),
                        "error should name the env var; got: {}",
                        msg
                    );
                },
            }
        })
        .await;
    }

    /// `anthropic` is dispatched as a separate path from the OpenAI-compat
    /// registry. Without `ANTHROPIC_API_KEY` set we should get a clear
    /// auth error pointing at the env var, not "Unknown provider".
    #[tokio::test]
    async fn anthropic_missing_api_key_returns_authentication_error() {
        temp_env::async_with_vars([("ANTHROPIC_API_KEY", None::<&str>)], async {
            let cfg = Config::default();
            let factory = ModelFactory::from_config(&cfg);
            match factory.create_model("anthropic/claude-sonnet-4-6").await {
                Ok(_) => panic!("expected auth error"),
                Err(e) => {
                    let msg = e.to_string();
                    assert!(
                        msg.contains("ANTHROPIC_API_KEY"),
                        "error should name the env var; got: {}",
                        msg
                    );
                    assert!(
                        !msg.contains("Unknown provider"),
                        "anthropic must be a known provider; got: {}",
                        msg
                    );
                },
            }
        })
        .await;
    }

    /// `mermaid list` should surface a curated list of Claude models even
    /// though Anthropic has no public discovery endpoint. The adapter
    /// itself returns Unsupported; the curated list lives in
    /// `ModelFactory::list_models`.
    #[tokio::test]
    async fn anthropic_list_models_returns_curated_list() {
        let cfg = Config::default();
        let factory = ModelFactory::from_config(&cfg);
        let models = factory
            .list_models("anthropic")
            .await
            .expect("curated list should always succeed");
        assert!(
            models.iter().any(|m| m == "claude-sonnet-4-6"),
            "expected sonnet-4-6 in curated list; got {:?}",
            models
        );
        assert!(
            models.iter().any(|m| m == "claude-opus-4-7"),
            "expected opus-4-7 in curated list; got {:?}",
            models
        );
    }

    /// Same auth-error contract for Gemini as Anthropic. Without
    /// `GOOGLE_API_KEY` we should get a clear pointer at the env var
    /// (and the override path), not "Unknown provider".
    #[tokio::test]
    async fn gemini_missing_api_key_returns_authentication_error() {
        temp_env::async_with_vars([("GOOGLE_API_KEY", None::<&str>)], async {
            let cfg = Config::default();
            let factory = ModelFactory::from_config(&cfg);
            // Use a current preview ID — `gemini-3-pro` was shut down
            // 2026-03-09 and would produce a different code path.
            match factory.create_model("gemini/gemini-3.1-pro-preview").await {
                Ok(_) => panic!("expected auth error"),
                Err(e) => {
                    let msg = e.to_string();
                    assert!(
                        msg.contains("GOOGLE_API_KEY"),
                        "error should name the env var; got: {}",
                        msg
                    );
                    assert!(
                        !msg.contains("Unknown provider"),
                        "gemini must be a known provider; got: {}",
                        msg
                    );
                },
            }
        })
        .await;
    }

    #[tokio::test]
    async fn gemini_list_models_returns_curated_list() {
        let cfg = Config::default();
        let factory = ModelFactory::from_config(&cfg);
        let models = factory
            .list_models("gemini")
            .await
            .expect("curated list should always succeed");
        // The `-latest` aliases are the user-friendly entry points —
        // they repoint as Google ships new previews, so we surface them
        // first.
        assert!(
            models.iter().any(|m| m == "gemini-pro-latest"),
            "expected gemini-pro-latest alias in curated list; got {:?}",
            models
        );
        // A concrete preview ID verified as of 2026-04-16.
        assert!(
            models.iter().any(|m| m == "gemini-3.1-pro-preview"),
            "expected gemini-3.1-pro-preview in curated list; got {:?}",
            models
        );
        // The deprecated bare `gemini-3-pro` must NOT be in the list —
        // it's not a valid API ID and its preview was shut down in
        // 2026-03.
        assert!(
            !models.iter().any(|m| m == "gemini-3-pro"),
            "gemini-3-pro is not a valid API ID as of 2026-04; \
             must not be in curated list; got {:?}",
            models
        );
    }
}
