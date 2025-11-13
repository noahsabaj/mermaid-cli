/// Unified model implementation
///
/// This is the primary Model implementation that wraps the backend router
/// and provides the clean public API. It handles backend discovery, routing,
/// and lifecycle management transparently.

use async_trait::async_trait;
use std::sync::Arc;

use super::backend::Backend;
use super::config::{BackendConfig, ModelConfig};
use super::error::Result;
use super::router::BackendRouter;
use super::traits::Model;
use super::types::{ChatMessage, ModelResponse, ProjectContext, StreamCallback};

/// Unified model implementation
///
/// This is what users create via ModelFactory. It transparently handles
/// backend selection, discovery, and routing.
pub struct UnifiedModel {
    /// Model specification (e.g., "ollama/qwen3-coder:30b" or just "qwen3-coder:30b")
    model_spec: String,

    /// Smart router for backend discovery
    router: Arc<BackendRouter>,

    /// Cached backend (lazy initialization)
    cached_backend: Option<(Arc<dyn Backend>, String)>,
}

impl UnifiedModel {
    /// Create a new unified model
    pub fn new(model_spec: &str, config: BackendConfig) -> Self {
        Self {
            model_spec: model_spec.to_string(),
            router: Arc::new(BackendRouter::new(config)),
            cached_backend: None,
        }
    }

    /// Get or resolve the backend (lazy initialization)
    async fn get_backend(&mut self) -> Result<(Arc<dyn Backend>, String)> {
        // Return cached backend if available
        if let Some(ref cached) = self.cached_backend {
            return Ok(cached.clone());
        }

        // Resolve backend via router
        let (backend, model_name) = self.router.resolve_model(&self.model_spec).await?;

        // Cache for future use
        self.cached_backend = Some((backend.clone(), model_name.clone()));

        Ok((backend, model_name))
    }
}

#[async_trait]
impl Model for UnifiedModel {
    async fn chat(
        &mut self,
        messages: &[ChatMessage],
        context: &ProjectContext,
        config: &ModelConfig,
        stream_callback: Option<StreamCallback>,
    ) -> Result<ModelResponse> {
        // Get backend (lazy initialization + caching)
        let (backend, model_name) = self.get_backend().await?;

        // Delegate to backend
        backend.chat(&model_name, messages, context, config, stream_callback).await
    }

    fn name(&self) -> &str {
        &self.model_spec
    }

    fn is_local(&self) -> bool {
        // Assume local unless it's explicitly an API provider
        // This will be accurate after first backend resolution
        if let Some((ref backend, _)) = self.cached_backend {
            backend.metadata().is_local
        } else {
            // Conservative default: assume not local until we know
            !self.model_spec.starts_with("openai/")
                && !self.model_spec.starts_with("anthropic/")
                && !self.model_spec.starts_with("groq/")
        }
    }

    async fn validate_connection(&self) -> Result<bool> {
        // Quick validation without resolving if not cached yet
        if let Some((ref backend, _)) = self.cached_backend {
            Ok(backend.health_check().await.is_ok())
        } else {
            // Can't validate without resolving - return optimistic true
            // Real validation happens on first chat() call
            Ok(true)
        }
    }
}

/// Factory functions for creating unified models

/// Create a model from a model specification
///
/// Examples:
/// - "ollama/qwen3-coder:30b" - Explicit backend
/// - "qwen3-coder:30b" - Auto-detect backend
/// - "gpt-4" - Search backends
pub async fn create_model(model_spec: &str, backend_config: BackendConfig) -> Result<Box<dyn Model>> {
    Ok(Box::new(UnifiedModel::new(model_spec, backend_config)))
}

/// Create a model with default configuration
pub async fn create_model_default(model_spec: &str) -> Result<Box<dyn Model>> {
    create_model(model_spec, BackendConfig::default()).await
}
