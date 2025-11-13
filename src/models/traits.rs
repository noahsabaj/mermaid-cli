/// Core Model trait - the public API for model interactions
///
/// This is what external code uses. Internally, it wraps the Backend system.

use async_trait::async_trait;

use super::config::ModelConfig;
use super::error::Result;
use super::types::{ChatMessage, ModelResponse, ProjectContext, StreamCallback};

/// Core trait that all model implementations must provide
///
/// This is the public API. Internal backends implement this via UnifiedModel wrapper.
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

    /// Check if this is a local model (no external API calls)
    fn is_local(&self) -> bool;

    /// Validate that the model is accessible
    async fn validate_connection(&self) -> Result<bool> {
        Ok(true)
    }
}
