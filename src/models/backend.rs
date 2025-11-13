/// Core backend abstraction layer
///
/// This module defines the internal Backend trait that all provider adapters implement.
/// It provides a clean, provider-agnostic interface for model inference operations.

use async_trait::async_trait;
use std::sync::Arc;

use super::config::{BackendConfig, ModelConfig};
use super::error::Result;
use super::types::{ChatMessage, ModelResponse, ProjectContext, StreamCallback};

/// Core backend trait - implemented by all provider adapters
///
/// This is the internal abstraction. External code uses the Model trait,
/// which is implemented by wrapping Backend implementations.
#[async_trait]
pub trait Backend: Send + Sync {
    /// Get the backend name (e.g., "ollama", "vllm", "openai")
    fn name(&self) -> &str;

    /// Check if this backend is available (running, reachable)
    async fn health_check(&self) -> Result<()>;

    /// List available models from this backend
    async fn list_models(&self) -> Result<Vec<String>>;

    /// Check if a specific model is available
    async fn has_model(&self, model_name: &str) -> Result<bool> {
        Ok(self.list_models().await?.contains(&model_name.to_string()))
    }

    /// Send a chat request to the model
    async fn chat(
        &self,
        model_name: &str,
        messages: &[ChatMessage],
        context: &ProjectContext,
        config: &ModelConfig,
        stream_callback: Option<StreamCallback>,
    ) -> Result<ModelResponse>;

    /// Get backend-specific metadata
    fn metadata(&self) -> BackendMetadata {
        BackendMetadata::default()
    }

    /// Shutdown the backend gracefully
    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}

/// Backend metadata (capabilities, limits, etc)
#[derive(Debug, Clone)]
pub struct BackendMetadata {
    /// Maximum context length supported
    pub max_context_length: usize,

    /// Supports streaming responses
    pub supports_streaming: bool,

    /// Supports function calling
    pub supports_functions: bool,

    /// Supports vision/images
    pub supports_vision: bool,

    /// Is a local backend (no API calls to external services)
    pub is_local: bool,

    /// Backend version (if available)
    pub version: Option<String>,
}

impl Default for BackendMetadata {
    fn default() -> Self {
        Self {
            max_context_length: 4096,
            supports_streaming: true,
            supports_functions: false,
            supports_vision: false,
            is_local: false,
            version: None,
        }
    }
}

/// Backend factory - creates backend instances
pub struct BackendFactory {
    config: Arc<BackendConfig>,
}

impl BackendFactory {
    /// Create a new backend factory
    pub fn new(config: BackendConfig) -> Self {
        Self {
            config: Arc::new(config),
        }
    }

    /// Create a backend by name
    pub async fn create_backend(&self, backend_name: &str) -> Result<Arc<dyn Backend>> {
        match backend_name.to_lowercase().as_str() {
            "ollama" => {
                use super::adapters::ollama::OllamaAdapter;
                let adapter = OllamaAdapter::new(self.config.clone()).await?;
                Ok(Arc::new(adapter))
            }
            "vllm" => {
                use super::adapters::vllm::VLLMAdapter;
                let adapter = VLLMAdapter::new(self.config.clone()).await?;
                Ok(Arc::new(adapter))
            }
            _ => Err(super::error::ModelError::InvalidRequest(
                format!("Unknown backend: {}", backend_name),
            )),
        }
    }

    /// Get all available backends
    pub async fn available_backends(&self) -> Vec<String> {
        let mut backends = Vec::new();

        // Try Ollama
        if let Ok(backend) = self.create_backend("ollama").await {
            if backend.health_check().await.is_ok() {
                backends.push("ollama".to_string());
            }
        }

        // Try vLLM
        if let Ok(backend) = self.create_backend("vllm").await {
            if backend.health_check().await.is_ok() {
                backends.push("vllm".to_string());
            }
        }

        backends
    }
}

/// Connection pool wrapper for backends
///
/// Handles connection lifecycle, pooling, and health monitoring
pub struct PooledBackend {
    inner: Arc<dyn Backend>,
    pool_config: PoolConfig,
}

#[derive(Debug, Clone)]
pub struct PoolConfig {
    pub max_connections: usize,
    pub idle_timeout_secs: u64,
    pub health_check_interval_secs: u64,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            max_connections: 10,
            idle_timeout_secs: 90,
            health_check_interval_secs: 30,
        }
    }
}

impl PooledBackend {
    pub fn new(backend: Arc<dyn Backend>, pool_config: PoolConfig) -> Self {
        Self {
            inner: backend,
            pool_config,
        }
    }

    pub fn inner(&self) -> &Arc<dyn Backend> {
        &self.inner
    }
}
