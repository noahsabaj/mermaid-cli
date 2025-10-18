/// Unified model interface that intelligently routes to the best available backend
///
/// Provides a simple single-backend interface that automatically selects the right
/// backend (Ollama, vLLM, etc) for a given model. Acts as a facade over BackendRegistry.

use anyhow::Result;
use std::sync::Arc;

use super::backend::ModelBackend;
use super::registry::BackendRegistry;
use super::types::{ChatMessage, ModelConfig, ModelResponse, StreamCallback};

/// Unified model interface that auto-selects backend
pub struct UnifiedBackend {
    registry: BackendRegistry,
    model_name: String,
}

impl UnifiedBackend {
    /// Create a new unified backend for a specific model
    pub async fn new(registry: BackendRegistry, model_spec: &str) -> Result<Self> {
        Ok(Self {
            registry,
            model_name: model_spec.to_string(),
        })
    }

    /// Get the backend that will be used for this model
    async fn get_backend(&self) -> Result<Arc<dyn ModelBackend>> {
        self.registry.find_backend_for_model(&self.model_name).await
    }

    /// Generate a streaming response
    pub async fn generate_stream(
        &self,
        messages: &[ChatMessage],
        config: &ModelConfig,
        callback: StreamCallback,
    ) -> Result<ModelResponse> {
        let backend = self.get_backend().await?;
        backend
            .generate_stream(&self.model_name, messages, config, callback)
            .await
    }

    /// Get the actual model name on the backend
    pub fn model_name(&self) -> &str {
        &self.model_name
    }

    /// List all available models across all backends
    pub async fn list_all_models(&self) -> Result<Vec<String>> {
        let models = self.registry.list_all_models().await?;
        let mut model_list: Vec<_> = models.into_keys().collect();
        model_list.sort();
        Ok(model_list)
    }

    /// Get which backend is being used for this model
    pub async fn get_backend_name(&self) -> Result<String> {
        let backend = self.get_backend().await?;
        Ok(backend.backend_name().to_string())
    }

    /// Invalidate the model cache (force refresh)
    pub async fn refresh_models(&self) {
        self.registry.invalidate_cache().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unified_backend_model_name() {
        let registry = BackendRegistry::new();
        let backend = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(async { UnifiedBackend::new(registry, "test-model").await })
            .unwrap();

        assert_eq!(backend.model_name(), "test-model", "Should return correct model name");
    }

    #[tokio::test]
    async fn test_unified_backend_list_models_empty_registry() {
        let registry = BackendRegistry::new();
        let backend = UnifiedBackend::new(registry, "any-model").await.unwrap();

        let models = backend.list_all_models().await.unwrap();
        assert!(models.is_empty(), "Empty registry should return no models");
    }
}
