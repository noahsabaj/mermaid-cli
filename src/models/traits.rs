//! Core Model trait - the single interface for model interactions
//!
//! Adapters implement this trait directly. No intermediate layers.

use async_trait::async_trait;

use super::config::ModelConfig;
use super::error::Result;
use super::types::{ChatMessage, ModelResponse, StreamCallback};

/// Core trait that all model adapters implement
///
/// This is the only abstraction layer between user code and model providers.
#[async_trait]
pub trait Model: Send + Sync {
    /// Send a chat conversation to the model and get a response
    async fn chat(
        &self,
        messages: &[ChatMessage],
        config: &ModelConfig,
        stream_callback: Option<StreamCallback>,
    ) -> Result<ModelResponse>;

    /// Get the model identifier (e.g., "ollama/tinyllama")
    fn name(&self) -> &str;

    /// List available models from this backend
    async fn list_models(&self) -> Result<Vec<String>>;
}
