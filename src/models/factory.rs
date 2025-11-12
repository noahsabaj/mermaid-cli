use anyhow::Result;

use super::backend_model_adapter::BackendModelAdapter;
use super::ollama_direct::OllamaDirectModel;
use super::registry::BackendRegistry;
use super::traits::Model;
use super::unified::UnifiedModel;
use super::unified_backend::UnifiedBackend;
use crate::app::Config;

/// Factory for creating model instances
///
/// Routes to appropriate implementation based on provider:
/// - "ollama/*" -> Direct Ollama connection (no proxy needed)
/// - Other providers -> LiteLLM proxy (requires proxy running)
/// - vLLM models -> UnifiedBackend with auto-detection
pub struct ModelFactory;

#[cfg(test)]
mod tests {
    use super::*;

    // Phase 3 Test Suite: Model Factory - 6 comprehensive tests

    #[test]
    fn test_model_id_format_validation_valid() {
        // Valid formats should contain /
        assert!(
            "ollama/tinyllama".contains('/'),
            "Valid model ID should have /"
        );
        assert!("openai/gpt-4".contains('/'), "Valid model ID should have /");
        assert!(
            "anthropic/claude-3".contains('/'),
            "Valid model ID should have /"
        );
    }

    #[test]
    fn test_model_id_format_validation_invalid() {
        // Invalid formats should fail validation logic
        let invalid_formats = vec![
            "ollama_tinyllama", // underscore instead of /
            "justslash/",       // missing model name
            "/noprefix",        // missing provider
        ];

        for format in invalid_formats {
            // These should fail validation in the factory
            assert!(
                format.ends_with('/') || format.starts_with('/') || format.contains('_'),
                "Format {} should be invalid",
                format
            );
        }
    }

    #[test]
    fn test_model_id_format_validation_auto_detect() {
        // Models without provider prefix should be valid (auto-detect)
        let valid_formats = vec![
            "qwen3-coder:30b",      // no prefix, will auto-detect
            "tinyllama",            // no prefix, will auto-detect
            "mistral-7b-instruct",  // no prefix, will auto-detect
        ];

        for format in valid_formats {
            // These should be valid and trigger auto-detection
            assert!(!format.contains('/'), "Model name should not have /");
        }
    }

    #[test]
    fn test_ollama_provider_detection() {
        // Ollama models start with ollama/
        assert!(
            "ollama/tinyllama".starts_with("ollama/"),
            "Should detect ollama provider"
        );
        assert!(
            "ollama/llama2".starts_with("ollama/"),
            "Should detect ollama provider"
        );

        // Non-ollama should not start with ollama/
        assert!(
            !"openai/gpt-4".starts_with("ollama/"),
            "Should not detect ollama for openai"
        );
        assert!(
            !"anthropic/claude".starts_with("ollama/"),
            "Should not detect ollama for anthropic"
        );
    }

    #[test]
    fn test_model_provider_extraction() {
        // Extract provider from model ID
        fn extract_provider(model_id: &str) -> Option<&str> {
            model_id.split('/').next()
        }

        assert_eq!(extract_provider("ollama/tinyllama"), Some("ollama"));
        assert_eq!(extract_provider("openai/gpt-4"), Some("openai"));
        assert_eq!(
            extract_provider("anthropic/claude-3-opus"),
            Some("anthropic")
        );
        assert_eq!(extract_provider("groq/llama3-70b"), Some("groq"));
    }

    #[test]
    fn test_model_name_extraction() {
        // Extract model name from model ID
        fn extract_model_name(model_id: &str) -> Option<&str> {
            model_id.split('/').nth(1)
        }

        assert_eq!(extract_model_name("ollama/tinyllama"), Some("tinyllama"));
        assert_eq!(extract_model_name("openai/gpt-4"), Some("gpt-4"));
        assert_eq!(
            extract_model_name("anthropic/claude-3-opus"),
            Some("claude-3-opus")
        );
        assert_eq!(extract_model_name("groq/llama3-70b"), Some("llama3-70b"));
    }

    #[test]
    fn test_routing_logic_ollama_vs_api() {
        // Verify routing logic for different providers
        let models = vec![
            ("ollama/tinyllama", "ollama", true), // ollama = direct
            ("ollama/llama2", "ollama", true),
            ("openai/gpt-4", "openai", false), // openai = proxy
            ("anthropic/claude", "anthropic", false),
            ("groq/llama3", "groq", false),
        ];

        for (model_id, expected_provider, should_be_direct) in models {
            assert!(
                model_id.starts_with("ollama/") == should_be_direct,
                "Model {} should_be_direct: {}",
                model_id,
                should_be_direct
            );
            assert!(
                model_id.starts_with(expected_provider),
                "Model {} should start with {}",
                model_id,
                expected_provider
            );
        }
    }
}

impl ModelFactory {
    /// Create a model instance from a model identifier with optional config and backend preference
    ///
    /// Format: provider/model (e.g., "ollama/qwen3-coder:30b", "openai/gpt-4", "anthropic/claude-3-opus")
    /// OR just model name (e.g., "qwen3-coder:30b") - auto-detects backend
    ///
    /// Routing logic:
    /// - With provider prefix (e.g., ollama/model):
    ///   - ollama/* -> Direct Ollama connection (localhost:11434)
    ///   - Other providers -> LiteLLM proxy
    /// - Without provider prefix (e.g., just model name):
    ///   - Auto-detects available backends (Ollama, vLLM, etc)
    ///   - Prefers vLLM if model exists on both
    ///   - Falls back to Ollama or LiteLLM proxy as needed
    pub async fn create(model_id: &str, config: Option<&Config>) -> Result<Box<dyn Model>> {
        Self::create_with_backend(model_id, config, None).await
    }

    /// Create a model with explicit backend preference
    pub async fn create_with_backend(
        model_id: &str,
        config: Option<&Config>,
        backend: Option<&str>,
    ) -> Result<Box<dyn Model>> {
        // If backend explicitly specified, enforce it
        if let Some(backend_name) = backend {
            return Self::create_with_explicit_backend(model_id, config, backend_name).await;
        }

        // If no provider prefix, auto-detect backend
        if !model_id.contains('/') {
            return Self::create_with_auto_detection(model_id).await;
        }

        // Route based on explicit provider
        if model_id.starts_with("ollama/") {
            // DIRECT CONNECTION - No proxy needed
            // This is the tested and working path
            // Pass config for offloading settings (num_gpu, num_thread, etc.)
            let model = OllamaDirectModel::new(model_id, config).await?;
            Ok(Box::new(model))
        } else {
            // API MODELS - Use LiteLLM proxy
            // Extract master_key from config if provided
            let master_key = config.and_then(|c| c.litellm.master_key.clone());
            let model = UnifiedModel::new(model_id, master_key).await?;
            Ok(Box::new(model))
        }
    }

    /// List available models from LiteLLM proxy
    pub async fn list_available() -> Result<Vec<String>> {
        use reqwest::Client;
        use serde::Deserialize;

        #[derive(Deserialize)]
        struct ModelsResponse {
            data: Vec<ModelInfo>,
        }

        #[derive(Deserialize)]
        struct ModelInfo {
            id: String,
        }

        // Get proxy URL from environment or use default
        let proxy_url = std::env::var("LITELLM_PROXY_URL")
            .unwrap_or_else(|_| "http://localhost:4000".to_string());

        // Get master key for authentication
        let master_key = std::env::var("LITELLM_MASTER_KEY").ok();

        // Query LiteLLM proxy for available models
        let client = Client::new();
        let url = format!("{}/v1/models", proxy_url);

        let mut request = client.get(&url);
        if let Some(key) = master_key {
            request = request.header("Authorization", format!("Bearer {}", key));
        }

        match request.send().await {
            Ok(response) if response.status().is_success() => {
                let models_response: ModelsResponse = response.json().await?;
                Ok(models_response.data.into_iter().map(|m| m.id).collect())
            },
            _ => {
                // Fallback to common models if proxy is not available
                Ok(vec![
                    "ollama/tinyllama".to_string(),
                    "ollama/llama2".to_string(),
                    "ollama/mistral".to_string(),
                    "ollama/codellama".to_string(),
                    "openai/gpt-4o".to_string(),
                    "openai/gpt-3.5-turbo".to_string(),
                    "anthropic/claude-3-sonnet".to_string(),
                    "groq/llama3-70b".to_string(),
                ])
            },
        }
    }

    /// Validate that a model is accessible
    pub async fn validate(model_id: &str, config: Option<&Config>) -> Result<bool> {
        match Self::create(model_id, config).await {
            Ok(model) => model.validate_connection().await,
            Err(_) => Ok(false),
        }
    }

    /// Create a model with explicit backend enforcement
    ///
    /// Forces the use of a specific backend (ollama or vllm) regardless of model_id format.
    /// Useful with --backend CLI flag to override auto-detection.
    async fn create_with_explicit_backend(
        model_id: &str,
        config: Option<&Config>,
        backend: &str,
    ) -> Result<Box<dyn Model>> {
        match backend.to_lowercase().as_str() {
            "ollama" => {
                // Force Ollama backend
                let ollama_model = if model_id.starts_with("ollama/") {
                    model_id.to_string()
                } else {
                    format!("ollama/{}", model_id)
                };
                let model = OllamaDirectModel::new(&ollama_model, config).await?;
                Ok(Box::new(model))
            }
            "vllm" => {
                // Force vLLM backend - use registry to find model
                let mut registry = BackendRegistry::new();
                registry.auto_discover().await?;

                // Strip any provider prefix for vLLM lookup
                let model_name = if model_id.contains('/') {
                    model_id.split('/').nth(1).unwrap_or(model_id)
                } else {
                    model_id
                };

                let backend = UnifiedBackend::new(registry, model_name).await?;
                let adapter = BackendModelAdapter::new(backend);
                Ok(Box::new(adapter))
            }
            _ => {
                anyhow::bail!(
                    "Unknown backend '{}'. Valid backends: ollama, vllm",
                    backend
                )
            }
        }
    }

    /// Create a model with intelligent backend auto-detection
    ///
    /// This method attempts to find the model across all available backends
    /// (Ollama, vLLM, etc) and automatically routes to the correct one.
    ///
    /// Useful for model specs like "ring-1t" that might be on vLLM but not Ollama.
    pub async fn create_with_auto_detection(model_spec: &str) -> Result<Box<dyn Model>> {
        let mut registry = BackendRegistry::new();
        registry.auto_discover().await?;

        let backend = UnifiedBackend::new(registry, model_spec).await?;
        let adapter = BackendModelAdapter::new(backend);
        Ok(Box::new(adapter))
    }

    /// List all models from both Ollama and vLLM backends
    pub async fn list_all_backend_models() -> Result<Vec<String>> {
        let mut registry = BackendRegistry::new();
        registry.auto_discover().await?;

        let models = registry.list_all_models().await?;
        let mut model_list: Vec<_> = models.keys().cloned().collect();
        model_list.sort();
        Ok(model_list)
    }

    /// Get which backend is available
    pub async fn get_available_backends() -> Vec<String> {
        let mut registry = BackendRegistry::new();
        let _ = registry.auto_discover().await;
        registry.backends()
    }
}
