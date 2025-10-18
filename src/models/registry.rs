/// Backend registry for intelligent auto-detection and model resolution
///
/// Manages discovery of available backends (Ollama, vLLM, etc) and intelligently
/// routes model requests to the appropriate backend. Maintains cache of model availability
/// to minimize repeated backend queries.

use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use super::backend::ModelBackend;
use super::backends::{OllamaBackend, VLLMBackend};

/// Represents a single backend with metadata
#[derive(Clone)]
struct BackendEntry {
    backend: Arc<dyn ModelBackend>,
    name: String,
}

impl std::fmt::Debug for BackendEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackendEntry")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// Cached model availability information
#[derive(Clone, Debug)]
struct ModelCache {
    models: HashMap<String, String>,
    last_updated: std::time::Instant,
}

/// Central registry for model backends with intelligent routing
pub struct BackendRegistry {
    backends: Vec<BackendEntry>,
    model_cache: Arc<RwLock<Option<ModelCache>>>,
    cache_ttl: std::time::Duration,
}

impl BackendRegistry {
    /// Create a new registry
    pub fn new() -> Self {
        Self {
            backends: Vec::new(),
            model_cache: Arc::new(RwLock::new(None)),
            cache_ttl: std::time::Duration::from_secs(300),
        }
    }

    /// Register a backend in the registry
    pub fn register(&mut self, backend: Arc<dyn ModelBackend>) -> Result<()> {
        let name = backend.backend_name().to_string();
        self.backends.push(BackendEntry {
            backend: backend.clone(),
            name: name.clone(),
        });

        Ok(())
    }

    /// Auto-discover and register available backends
    pub async fn auto_discover(&mut self) -> Result<()> {
        let mut discovered = Vec::new();

        if let Ok(ollama) = OllamaBackend::new(None, None).await {
            discovered.push((
                Arc::new(ollama) as Arc<dyn ModelBackend>,
                "Ollama".to_string(),
            ));
        }

        if let Ok(vllm) = VLLMBackend::new(None).await {
            discovered.push((
                Arc::new(vllm) as Arc<dyn ModelBackend>,
                "vLLM".to_string(),
            ));
        }

        for (backend, name) in discovered {
            self.backends.push(BackendEntry {
                backend,
                name,
            });
        }

        Ok(())
    }

    /// Get all available backends
    pub fn backends(&self) -> Vec<String> {
        self.backends.iter().map(|b| b.name.clone()).collect()
    }

    /// List all available models across all backends
    pub async fn list_all_models(&self) -> Result<HashMap<String, String>> {
        let cache = self.model_cache.read().await;

        if let Some(cache) = cache.as_ref() {
            if cache.last_updated.elapsed() < self.cache_ttl {
                return Ok(cache.models.clone());
            }
        }

        drop(cache);

        let mut models = HashMap::new();

        for backend_entry in &self.backends {
            match backend_entry.backend.list_models().await {
                Ok(backend_models) => {
                    for model in backend_models {
                        models.insert(model, backend_entry.name.clone());
                    }
                }
                Err(_) => {
                    continue;
                }
            }
        }

        let mut cache = self.model_cache.write().await;
        *cache = Some(ModelCache {
            models: models.clone(),
            last_updated: std::time::Instant::now(),
        });

        Ok(models)
    }

    /// Find the best backend for a specific model
    pub async fn find_backend_for_model(&self, model_name: &str) -> Result<Arc<dyn ModelBackend>> {
        let models = self.list_all_models().await?;

        if let Some(backend_name) = models.get(model_name) {
            for entry in &self.backends {
                if entry.name == *backend_name {
                    return Ok(entry.backend.clone());
                }
            }
        }

        anyhow::bail!(
            "Model '{}' not found on any backend. Available backends: {:?}",
            model_name,
            self.backends()
        )
    }

    /// Find backend by name
    pub fn get_backend(&self, backend_name: &str) -> Result<Arc<dyn ModelBackend>> {
        for entry in &self.backends {
            if entry.name == backend_name {
                return Ok(entry.backend.clone());
            }
        }

        anyhow::bail!(
            "Backend '{}' not found. Available: {:?}",
            backend_name,
            self.backends()
        )
    }

    /// Get backend for model with fallback to full model name matching
    pub async fn resolve_model(
        &self,
        model_spec: &str,
    ) -> Result<(Arc<dyn ModelBackend>, String)> {
        let model_spec = model_spec.trim();

        let models = self.list_all_models().await?;

        for (available_model, backend_name) in &models {
            if available_model.contains(model_spec) || model_spec.contains(available_model) {
                let backend = self.get_backend(backend_name)?;
                return Ok((backend, available_model.clone()));
            }
        }

        anyhow::bail!(
            "Model '{}' not found on any backend. Try 'mermaid --list-models' to see available models",
            model_spec
        )
    }

    /// Invalidate the model cache to force refresh
    pub async fn invalidate_cache(&self) {
        let mut cache = self.model_cache.write().await;
        *cache = None;
    }
}

impl Default for BackendRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_registry_creation() {
        let registry = BackendRegistry::new();
        assert_eq!(registry.backends().len(), 0, "New registry should be empty");
    }

    #[test]
    fn test_registry_default() {
        let registry = BackendRegistry::default();
        assert_eq!(registry.backends().len(), 0, "Default registry should be empty");
    }

    #[test]
    fn test_backends_list_empty() {
        let registry = BackendRegistry::new();
        let backends = registry.backends();
        assert!(backends.is_empty(), "Empty registry should have no backends");
    }

    #[test]
    fn test_cache_ttl_configuration() {
        let registry = BackendRegistry::new();
        assert_eq!(
            registry.cache_ttl,
            std::time::Duration::from_secs(300),
            "Default cache TTL should be 300 seconds"
        );
    }
}
