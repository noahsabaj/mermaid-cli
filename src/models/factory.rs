/// Factory for creating model instances
///
/// This factory provides the public API for creating models. It handles
/// configuration conversion and delegates to the internal ModelFactory.

use super::backend::ModelFactory as InternalFactory;
use super::config::BackendConfig;
use super::error::Result;
use super::traits::Model;
use crate::app::Config;

/// Factory for creating model instances
pub struct ModelFactory;

impl ModelFactory {
    /// Create a model instance from a model identifier
    ///
    /// Format examples:
    /// - "ollama/qwen3-coder:30b" - Explicit Ollama provider
    /// - "qwen3-coder:30b" - Defaults to Ollama
    /// - "kimi-k2.5:cloud" - Ollama cloud model
    pub async fn create(model_id: &str, config: Option<&Config>) -> Result<Box<dyn Model>> {
        let backend_config = if let Some(cfg) = config {
            Self::config_to_backend_config(cfg)
        } else {
            BackendConfig::default()
        };

        let factory = InternalFactory::new(backend_config);
        factory.create_model(model_id).await
    }

    /// Create a model with default configuration
    pub async fn create_default(model_id: &str) -> Result<Box<dyn Model>> {
        let factory = InternalFactory::new(BackendConfig::default());
        factory.create_model(model_id).await
    }

    /// Create a model with explicit provider preference
    ///
    /// If provider is specified, the model_id will be prefixed with the provider name
    /// if it's not already specified. For example:
    /// - provider="ollama", model_id="tinyllama" -> "ollama/tinyllama"
    /// - provider=None, model_id="qwen3-coder:30b" -> defaults to ollama
    pub async fn create_with_provider(
        model_id: &str,
        config: Option<&Config>,
        provider: Option<&str>,
    ) -> Result<Box<dyn Model>> {
        let backend_config = if let Some(cfg) = config {
            Self::config_to_backend_config(cfg)
        } else {
            BackendConfig::default()
        };

        // If provider is explicitly specified, prefix the model_id
        let final_model_id = if let Some(provider_name) = provider {
            if model_id.contains('/') {
                // Already has a provider prefix
                model_id.to_string()
            } else {
                // Add provider prefix
                format!("{}/{}", provider_name, model_id)
            }
        } else {
            model_id.to_string()
        };

        let factory = InternalFactory::new(backend_config);
        factory.create_model(&final_model_id).await
    }

    /// Create a model with explicit backend preference (alias for create_with_provider)
    pub async fn create_with_backend(
        model_id: &str,
        config: Option<&Config>,
        backend: Option<&str>,
    ) -> Result<Box<dyn Model>> {
        Self::create_with_provider(model_id, config, backend).await
    }

    /// Get available backends (providers)
    pub async fn get_available_backends() -> Vec<String> {
        let factory = InternalFactory::new(BackendConfig::default());
        factory.available_providers().await
    }

    /// List all models from all available backends
    ///
    /// Returns a list of model identifiers in "provider/model" format.
    /// Only includes backends that are currently available.
    pub async fn list_all_backend_models() -> Result<Vec<String>> {
        let factory = InternalFactory::new(BackendConfig::default());
        let providers = factory.available_providers().await;

        let mut all_models = Vec::new();

        for provider in providers {
            // Create a dummy model to list models from this provider
            let dummy_model_id = format!("{}/dummy", provider);
            if let Ok(model) = factory.create_model(&dummy_model_id).await {
                if let Ok(models) = model.list_models().await {
                    for model_name in models {
                        all_models.push(format!("{}/{}", provider, model_name));
                    }
                }
            }
        }

        all_models.sort();
        Ok(all_models)
    }

    /// Convert app::Config to BackendConfig
    fn config_to_backend_config(config: &Config) -> BackendConfig {
        // Construct Ollama URL from host and port
        let ollama_url = format!("http://{}:{}", config.ollama.host, config.ollama.port);

        BackendConfig {
            ollama_url,
            timeout_secs: 10,
            request_timeout_secs: 120,
            max_idle_per_host: 10,
            health_check_interval_secs: 30,
        }
    }
}

#[cfg(test)]
mod tests {
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
