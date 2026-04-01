//! Model factory - creates model instances from identifiers
//!
//! Parses model identifiers like "ollama/llama3" and creates
//! the appropriate adapter implementing the Model trait.
//! Also provides static convenience methods for common operations.

use std::sync::Arc;
use std::time::Duration;

use super::config::BackendConfig;
use super::error::{BackendError, ModelError, Result};
use super::traits::Model;
use crate::app::Config;

/// Model factory - creates model instances
pub struct ModelFactory {
    config: Arc<BackendConfig>,
}

impl ModelFactory {
    /// Create a new model factory with explicit backend config
    pub fn new(config: BackendConfig) -> Self {
        Self {
            config: Arc::new(config),
        }
    }

    /// Create a factory from app::Config
    pub fn from_config(config: &Config) -> Self {
        Self::new(Self::config_to_backend_config(config))
    }

    /// Create a model from a full identifier (e.g., "ollama/llama3")
    pub async fn create_model(&self, model_id: &str) -> Result<Box<dyn Model>> {
        // Parse model identifier: "provider/model_name" or just "model_name" (defaults to ollama)
        let (provider, model_name) = parse_model_id(model_id);

        match provider.to_lowercase().as_str() {
            "ollama" => {
                use super::adapters::ollama::OllamaAdapter;
                let adapter = OllamaAdapter::new(model_name, self.config.clone()).await?;
                Ok(Box::new(adapter))
            },
            _ => Err(ModelError::InvalidRequest(format!(
                "Unknown provider: {}. Only ollama/ is supported.",
                provider
            ))),
        }
    }

    // --- Static convenience API (absorbed from factory.rs) ---

    /// Create a model instance from a model identifier with optional app config
    ///
    /// Format examples:
    /// - "ollama/qwen3-coder:30b" - Explicit Ollama provider
    /// - "qwen3-coder:30b" - Defaults to Ollama
    /// - "kimi-k2.5:cloud" - Ollama cloud model
    pub async fn create(model_id: &str, config: Option<&Config>) -> Result<Box<dyn Model>> {
        let backend_config = config
            .map(Self::config_to_backend_config)
            .unwrap_or_default();
        let factory = Self::new(backend_config);
        factory.create_model(model_id).await
    }

    /// List all models from all available providers
    ///
    /// Returns a list of model identifiers in "provider/model" format.
    /// Only includes providers that are currently available.
    pub async fn list_all_models() -> Result<Vec<String>> {
        let factory = Self::new(BackendConfig::default());
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

    /// List available providers with a fast single-shot health check
    async fn available_providers_impl(&self) -> Vec<String> {
        let mut providers = Vec::new();

        // Quick Ollama check: single GET with 2s timeout, no retries
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

        providers
    }

    /// List all models from a provider without creating a model instance
    pub async fn list_models(&self, provider: &str) -> Result<Vec<String>> {
        match provider {
            "ollama" => {
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
                Ok(tags.models.into_iter().map(|m| m.name).collect())
            },
            _ => Err(ModelError::InvalidRequest(format!(
                "Unknown provider: {}",
                provider
            ))),
        }
    }

    /// Convert app::Config to BackendConfig
    fn config_to_backend_config(config: &Config) -> BackendConfig {
        let ollama_url = format!("http://{}:{}", config.ollama.host, config.ollama.port);

        BackendConfig {
            ollama_url,
            timeout_secs: 10,
            max_idle_per_host: 10,
        }
    }
}

/// Parse a model identifier into provider and model name
///
/// Formats:
/// - "ollama/llama3" -> ("ollama", "llama3")
/// - "llama3" -> ("ollama", "llama3")  // defaults to ollama
/// - "llama3:latest" -> ("ollama", "llama3:latest")  // ollama tag format
fn parse_model_id(model_id: &str) -> (&str, &str) {
    if let Some(idx) = model_id.find('/') {
        // Safe: '/' is ASCII, so byte offset == char offset
        let provider = &model_id[..idx];
        let model = &model_id[idx + 1..];
        (provider, model)
    } else {
        // Default to ollama for bare model names
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

    #[test]
    fn test_model_spec_parsing() {
        // Test various model spec formats
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
}
