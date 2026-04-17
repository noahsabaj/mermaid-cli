//! Core Model trait - the single interface for model interactions
//!
//! Adapters implement this trait directly. No intermediate layers.

use async_trait::async_trait;

use super::capabilities::ModelCapabilities;
use super::config::ModelConfig;
use super::error::{ModelError, Result};
use super::stream::StreamCallback;
use super::types::{ChatMessage, ModelResponse};

/// Core trait that all model adapters implement.
///
/// `chat()` streams typed `StreamEvent`s (text, reasoning, tool calls,
/// completion) through the optional callback and returns a final
/// `ModelResponse`. Adapters own the translation between provider-native
/// stream shapes and the typed event surface — see
/// `OllamaAdapter::chat` for the reference impl.
#[async_trait]
pub trait Model: Send + Sync {
    /// Send a chat conversation to the model. If a callback is supplied
    /// the adapter streams typed events; otherwise it does a single
    /// blocking request and returns the response.
    async fn chat(
        &self,
        messages: &[ChatMessage],
        config: &ModelConfig,
        callback: Option<StreamCallback>,
    ) -> Result<ModelResponse>;

    /// Capabilities advertised by this model — does it support tools,
    /// vision, what reasoning controls, max context. Adapters return a
    /// `ModelCapabilities` populated at construction time.
    fn capabilities(&self) -> &ModelCapabilities;

    /// Get the model identifier (e.g., "ollama/tinyllama")
    fn name(&self) -> &str;

    /// List available models from this backend.
    ///
    /// Default impl returns `Unsupported` — appropriate for providers
    /// that have no `/models` endpoint (Anthropic, raw Bedrock). Ollama
    /// and OpenAI-compatible adapters override.
    async fn list_models(&self) -> Result<Vec<String>> {
        Err(ModelError::Unsupported {
            feature: "list_models".to_string(),
        })
    }
}
