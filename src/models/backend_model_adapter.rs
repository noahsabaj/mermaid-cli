/// Adapter that wraps UnifiedBackend to implement the Model trait
///
/// Bridges the backend system with the existing Model trait interface,
/// allowing UnifiedBackend to be used wherever Model is expected.

use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;

use super::traits::Model;
use super::types::{ChatMessage, ModelCapabilities, ModelConfig, ModelResponse, ProjectContext, StreamCallback};
use super::unified_backend::UnifiedBackend;

/// Model adapter wrapping UnifiedBackend
pub struct BackendModelAdapter {
    backend: UnifiedBackend,
}

impl BackendModelAdapter {
    /// Create a new model adapter from a unified backend
    pub fn new(backend: UnifiedBackend) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl Model for BackendModelAdapter {
    async fn chat(
        &mut self,
        messages: &[ChatMessage],
        _context: &ProjectContext,
        config: &ModelConfig,
        stream_callback: Option<StreamCallback>,
    ) -> Result<ModelResponse> {
        let callback: StreamCallback = stream_callback
            .unwrap_or_else(|| Arc::new(|_| {}));
        self.backend.generate_stream(messages, config, callback).await
    }

    fn name(&self) -> &str {
        self.backend.model_name()
    }

    fn is_local(&self) -> bool {
        true
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }

    async fn validate_connection(&self) -> Result<bool> {
        match self.backend.get_backend_name().await {
            Ok(backend_name) => {
                eprintln!("Using backend: {}", backend_name);
                Ok(true)
            }
            Err(_) => Ok(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_adapter_creation() {
        let registry = super::super::registry::BackendRegistry::new();
        let backend = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(async { UnifiedBackend::new(registry, "test-model").await })
            .unwrap();

        let adapter = BackendModelAdapter::new(backend);
        assert_eq!(adapter.name(), "test-model", "Adapter should expose model name");
    }

    #[test]
    fn test_adapter_is_local() {
        let registry = super::super::registry::BackendRegistry::new();
        let backend = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(async { UnifiedBackend::new(registry, "test-model").await })
            .unwrap();

        let adapter = BackendModelAdapter::new(backend);
        assert!(
            adapter.is_local(),
            "Backend models should be considered local"
        );
    }

    #[tokio::test]
    async fn test_adapter_validate_connection() {
        let registry = super::super::registry::BackendRegistry::new();
        let backend = UnifiedBackend::new(registry, "test-model").await.unwrap();
        let adapter = BackendModelAdapter::new(backend);

        let result = adapter.validate_connection().await;
        assert!(result.is_ok(), "Validation should not error");
    }
}
