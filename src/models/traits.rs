use anyhow::Result;
use async_trait::async_trait;

use super::types::{
    ChatMessage, ModelCapabilities, ModelConfig, ModelResponse, ProjectContext, StreamCallback,
};

/// Core trait that all model backends must implement
#[async_trait]
pub trait Model: Send + Sync {
    /// Send a chat conversation to the model and get a response
    async fn chat(
        &mut self,
        messages: &[ChatMessage],
        context: &ProjectContext,
        config: &ModelConfig,
        stream_callback: Option<StreamCallback>,
    ) -> Result<ModelResponse>;

    /// Get the name of the model
    fn name(&self) -> &str;

    /// Check if this is a local model (no API calls)
    fn is_local(&self) -> bool;

    /// Get model capabilities
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }

    /// Validate that the model is accessible
    async fn validate_connection(&self) -> Result<bool> {
        Ok(true)
    }
}
