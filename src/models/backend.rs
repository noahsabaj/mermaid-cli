/// Low-level backend abstraction for model inference
///
/// This trait defines the interface for different model backends (Ollama, vLLM, etc).
/// Backends handle the actual HTTP communication and model invocation.
/// Higher-level Model trait wraps backends to provide the public API.

use anyhow::Result;
use async_trait::async_trait;

use super::types::{ChatMessage, ModelConfig, ModelResponse, StreamCallback};

/// Represents a backend inference service (Ollama, vLLM, etc)
#[async_trait]
pub trait ModelBackend: Send + Sync {
    /// List all available models on this backend
    async fn list_models(&self) -> Result<Vec<String>>;

    /// Check if backend is available and healthy
    async fn health_check(&self) -> Result<()>;

    /// Get a human-readable name for this backend
    fn backend_name(&self) -> &str;

    /// Get the base URL for this backend
    fn base_url(&self) -> &str;

    /// Generate a streaming response from the model
    ///
    /// This is the core inference operation. The callback is called with each
    /// token as it's generated, allowing real-time streaming to the UI.
    async fn generate_stream(
        &self,
        model_name: &str,
        messages: &[ChatMessage],
        config: &ModelConfig,
        callback: StreamCallback,
    ) -> Result<ModelResponse>;

    /// Validate that a specific model exists on this backend
    async fn has_model(&self, model_name: &str) -> Result<bool> {
        let models = self.list_models().await?;
        Ok(models.iter().any(|m| m.contains(model_name)))
    }
}
