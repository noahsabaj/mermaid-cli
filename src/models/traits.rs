/// Core Model trait - the single interface for model interactions
///
/// Adapters implement this trait directly. No intermediate layers.

use async_trait::async_trait;

use super::config::ModelConfig;
use super::error::Result;
use super::types::{ChatMessage, ModelResponse, ProjectContext, StreamCallback};

/// Core trait that all model adapters implement
///
/// This is the only abstraction layer between user code and model providers.
#[async_trait]
pub trait Model: Send + Sync {
    /// Send a chat conversation to the model and get a response
    async fn chat(
        &self,
        messages: &[ChatMessage],
        context: &ProjectContext,
        config: &ModelConfig,
        stream_callback: Option<StreamCallback>,
    ) -> Result<ModelResponse>;

    /// Get the model identifier (e.g., "ollama/tinyllama")
    fn name(&self) -> &str;

    /// Check if this is a local model (no external API calls)
    fn is_local(&self) -> bool;

    /// Check if the model backend is available and healthy
    async fn health_check(&self) -> Result<()>;

    /// List available models from this backend
    async fn list_models(&self) -> Result<Vec<String>>;

    /// Check if a specific model is available
    async fn has_model(&self, model_name: &str) -> Result<bool> {
        let models = self.list_models().await?;
        // Check for exact match or prefix match (model:tag format)
        Ok(models.iter().any(|m| {
            m == model_name
                || m.starts_with(&format!("{}:", model_name))
                || model_name.starts_with(&format!("{}:", m))
        }))
    }

    /// Get model capabilities
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

/// Model capabilities (what the model/backend supports)
#[derive(Debug, Clone)]
pub struct ModelCapabilities {
    /// Maximum context length supported
    pub max_context_length: usize,
    /// Supports streaming responses
    pub supports_streaming: bool,
    /// Supports function/tool calling
    pub supports_functions: bool,
    /// Supports vision/images
    pub supports_vision: bool,
}

impl Default for ModelCapabilities {
    fn default() -> Self {
        Self {
            max_context_length: 4096,
            supports_streaming: true,
            supports_functions: false,
            supports_vision: false,
        }
    }
}
