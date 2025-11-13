/// Backend router with lazy discovery
///
/// Intelligently routes model requests to the appropriate backend without
/// heavy upfront scanning. Discovers backends on-demand and caches results.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use super::backend::{Backend, BackendFactory};
use super::config::BackendConfig;
use super::error::{ModelError, Result};

/// Smart router for backend discovery and selection
pub struct BackendRouter {
    factory: BackendFactory,
    /// Cache of model -> backend mappings
    model_cache: Arc<RwLock<HashMap<String, String>>>,
    /// Cache of available backends
    backend_cache: Arc<RwLock<Option<Vec<String>>>>,
}

impl BackendRouter {
    /// Create a new backend router
    pub fn new(config: BackendConfig) -> Self {
        Self {
            factory: BackendFactory::new(config),
            model_cache: Arc::new(RwLock::new(HashMap::new())),
            backend_cache: Arc::new(RwLock::new(None)),
        }
    }

    /// Resolve a model string to a backend
    ///
    /// Format examples:
    /// - "ollama/qwen3-coder:30b" -> Explicit backend
    /// - "qwen3-coder:30b" -> Search backends
    /// - "gpt-4" -> Search backends (likely vLLM or LiteLLM)
    pub async fn resolve_model(&self, model_spec: &str) -> Result<(Arc<dyn Backend>, String)> {
        // Parse model spec
        let (backend_hint, model_name) = parse_model_spec(model_spec);

        // If backend is explicitly specified, use it
        if let Some(backend_name) = backend_hint {
            let backend = self.factory.create_backend(backend_name).await?;
            return Ok((backend, model_name.to_string()));
        }

        // Check cache first
        {
            let cache = self.model_cache.read().await;
            if let Some(backend_name) = cache.get(model_name) {
                let backend = self.factory.create_backend(backend_name).await?;
                return Ok((backend, model_name.to_string()));
            }
        }

        // No cache hit - discover backends lazily
        let backend = self.discover_model(model_name).await?;
        Ok((backend, model_name.to_string()))
    }

    /// Discover which backend has a specific model
    async fn discover_model(&self, model_name: &str) -> Result<Arc<dyn Backend>> {
        // Try backends in priority order: Ollama, vLLM
        let backends_to_try = vec!["ollama", "vllm"];

        for backend_name in &backends_to_try {
            if let Ok(backend) = self.factory.create_backend(backend_name).await {
                // Check if backend is available
                if backend.health_check().await.is_ok() {
                    // Check if model exists
                    if let Ok(true) = backend.has_model(model_name).await {
                        // Cache the result
                        let mut cache = self.model_cache.write().await;
                        cache.insert(model_name.to_string(), backend_name.to_string());
                        return Ok(backend);
                    }
                }
            }
        }

        // Model not found on any backend
        let searched: Vec<String> = backends_to_try.iter().map(|s| s.to_string()).collect();
        Err(ModelError::ModelNotFound {
            model: model_name.to_string(),
            searched,
        })
    }

    /// Get list of available backends
    pub async fn available_backends(&self) -> Vec<String> {
        // Check cache first
        {
            let cache = self.backend_cache.read().await;
            if let Some(ref backends) = *cache {
                return backends.clone();
            }
        }

        // Discover available backends
        let backends = self.factory.available_backends().await;

        // Cache the result
        {
            let mut cache = self.backend_cache.write().await;
            *cache = Some(backends.clone());
        }

        backends
    }

    /// List all models from all available backends
    pub async fn list_all_models(&self) -> Result<HashMap<String, Vec<String>>> {
        let mut all_models = HashMap::new();
        let backends = self.available_backends().await;

        for backend_name in backends {
            if let Ok(backend) = self.factory.create_backend(&backend_name).await {
                if let Ok(models) = backend.list_models().await {
                    all_models.insert(backend_name, models);
                }
            }
        }

        Ok(all_models)
    }

    /// Clear caches (useful for testing or when backends change)
    pub async fn clear_cache(&self) {
        let mut model_cache = self.model_cache.write().await;
        model_cache.clear();

        let mut backend_cache = self.backend_cache.write().await;
        *backend_cache = None;
    }
}

/// Parse a model specification into (backend_hint, model_name)
///
/// Examples:
/// - "ollama/qwen3-coder:30b" -> (Some("ollama"), "qwen3-coder:30b")
/// - "qwen3-coder:30b" -> (None, "qwen3-coder:30b")
/// - "gpt-4" -> (None, "gpt-4")
fn parse_model_spec(spec: &str) -> (Option<&str>, &str) {
    if let Some(slash_pos) = spec.find('/') {
        let backend = &spec[..slash_pos];
        let model = &spec[slash_pos + 1..];
        (Some(backend), model)
    } else {
        (None, spec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_model_spec() {
        assert_eq!(
            parse_model_spec("ollama/tinyllama"),
            (Some("ollama"), "tinyllama")
        );
        assert_eq!(
            parse_model_spec("qwen3-coder:30b"),
            (None, "qwen3-coder:30b")
        );
        assert_eq!(parse_model_spec("gpt-4"), (None, "gpt-4"));
        assert_eq!(
            parse_model_spec("vllm/llama-3-70b"),
            (Some("vllm"), "llama-3-70b")
        );
    }
}
