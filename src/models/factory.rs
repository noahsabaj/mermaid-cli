/// Factory for creating model instances with unified backend architecture
///
/// This factory provides a simple API for creating models that automatically
/// handle backend discovery, routing, and configuration.

use super::config::BackendConfig;
use super::error::Result;
use super::model::{create_model, create_model_default};
use super::router::BackendRouter;
use super::traits::Model;
use crate::app::Config;

/// Factory for creating model instances
pub struct ModelFactory;

impl ModelFactory {
    /// Create a model instance from a model identifier
    ///
    /// Format examples:
    /// - "ollama/qwen3-coder:30b" - Explicit backend
    /// - "qwen3-coder:30b" - Auto-detect backend
    /// - "gpt-4" - Search backends (likely vLLM or LiteLLM)
    ///
    /// The factory will automatically discover available backends and route
    /// the request appropriately.
    pub async fn create(model_id: &str, config: Option<&Config>) -> Result<Box<dyn Model>> {
        let backend_config = if let Some(cfg) = config {
            Self::config_to_backend_config(cfg)
        } else {
            BackendConfig::default()
        };

        create_model(model_id, backend_config).await
    }

    /// Create a model with default configuration
    pub async fn create_default(model_id: &str) -> Result<Box<dyn Model>> {
        create_model_default(model_id).await
    }

    /// Create a model with explicit backend preference
    ///
    /// If backend is specified, the model_id will be prefixed with the backend name
    /// if it's not already specified. For example:
    /// - backend="ollama", model_id="tinyllama" -> "ollama/tinyllama"
    /// - backend="vllm", model_id="gpt-4" -> "vllm/gpt-4"
    /// - backend=None, model_id="qwen3-coder:30b" -> auto-detect backend
    pub async fn create_with_backend(
        model_id: &str,
        config: Option<&Config>,
        backend: Option<&str>,
    ) -> Result<Box<dyn Model>> {
        let backend_config = if let Some(cfg) = config {
            Self::config_to_backend_config(cfg)
        } else {
            BackendConfig::default()
        };

        // If backend is explicitly specified, prefix the model_id
        let final_model_id = if let Some(backend_name) = backend {
            if model_id.contains('/') {
                // Already has a provider prefix
                model_id.to_string()
            } else {
                // Add backend prefix
                format!("{}/{}", backend_name, model_id)
            }
        } else {
            model_id.to_string()
        };

        create_model(&final_model_id, backend_config).await
    }

    /// List all available models from all backends
    pub async fn list_all_backend_models() -> Result<Vec<String>> {
        let router = BackendRouter::new(BackendConfig::default());
        let all_models = router.list_all_models().await?;

        // Flatten the backend -> models map into a single list with backend prefix
        let mut model_list = Vec::new();
        for (backend_name, models) in all_models {
            for model in models {
                model_list.push(format!("{}/{}", backend_name, model));
            }
        }

        model_list.sort();
        Ok(model_list)
    }

    /// List available models (alias for list_all_backend_models)
    pub async fn list_available() -> Result<Vec<String>> {
        Self::list_all_backend_models().await
    }

    /// Get available backends
    pub async fn get_available_backends() -> Vec<String> {
        let router = BackendRouter::new(BackendConfig::default());
        router.available_backends().await
    }

    /// Validate that a model is accessible
    pub async fn validate(model_id: &str, config: Option<&Config>) -> Result<bool> {
        match Self::create(model_id, config).await {
            Ok(model) => model.validate_connection().await,
            Err(_) => Ok(false),
        }
    }

    /// Convert app::Config to BackendConfig
    fn config_to_backend_config(config: &Config) -> BackendConfig {
        // Construct Ollama URL from host and port
        let ollama_url = format!("http://{}:{}", config.ollama.host, config.ollama.port);

        BackendConfig {
            ollama_url,
            vllm_url: std::env::var("VLLM_API_BASE")
                .unwrap_or_else(|_| "http://localhost:8000".to_string()),
            litellm_url: config.litellm.proxy_url.clone(),
            litellm_master_key: config.litellm.master_key.clone(),
            timeout_secs: 10,
            request_timeout_secs: 120,
            max_idle_per_host: 10,
            health_check_interval_secs: 30,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_model_spec_parsing() {
        // Test various model spec formats
        let specs = vec![
            ("ollama/tinyllama", Some("ollama"), "tinyllama"),
            ("qwen3-coder:30b", None, "qwen3-coder:30b"),
            ("gpt-4", None, "gpt-4"),
        ];

        for (spec, expected_backend, expected_model) in specs {
            let parts: Vec<&str> = spec.split('/').collect();
            if parts.len() == 2 {
                assert_eq!(Some(parts[0]), expected_backend);
                assert_eq!(parts[1], expected_model);
            } else {
                assert_eq!(None, expected_backend);
                assert_eq!(spec, expected_model);
            }
        }
    }

    #[test]
    fn test_ollama_provider_detection() {
        assert!("ollama/tinyllama".starts_with("ollama/"));
        assert!("ollama/llama2".starts_with("ollama/"));
        assert!(!"openai/gpt-4".starts_with("ollama/"));
        assert!(!"qwen3-coder:30b".starts_with("ollama/"));
    }

    #[test]
    fn test_provider_extraction() {
        fn extract_provider(spec: &str) -> Option<&str> {
            spec.split('/').next().filter(|_| spec.contains('/'))
        }

        assert_eq!(extract_provider("ollama/tinyllama"), Some("ollama"));
        assert_eq!(extract_provider("vllm/gpt-4"), Some("vllm"));
        assert_eq!(extract_provider("qwen3-coder:30b"), None);
    }

    #[test]
    fn test_model_name_extraction() {
        fn extract_model(spec: &str) -> &str {
            if let Some(pos) = spec.find('/') {
                &spec[pos + 1..]
            } else {
                spec
            }
        }

        assert_eq!(extract_model("ollama/tinyllama"), "tinyllama");
        assert_eq!(extract_model("vllm/gpt-4"), "gpt-4");
        assert_eq!(extract_model("qwen3-coder:30b"), "qwen3-coder:30b");
    }
}
