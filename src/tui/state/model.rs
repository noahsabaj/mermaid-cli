//! Model state management
//!
//! Handles LLM configuration and identity.

use std::sync::Arc;
use tokio::sync::RwLock;

use crate::models::{Model, ModelConfig, OllamaOptions};

/// Model state - LLM configuration and identity
pub struct ModelState {
    pub model: Arc<RwLock<Box<dyn Model>>>,
    pub model_id: String,
    pub model_name: String,
    /// Thinking mode state:
    /// - Some(true) = model supports thinking, currently enabled
    /// - Some(false) = model supports thinking, currently disabled
    /// - None = model does not support thinking (or unknown)
    pub thinking_enabled: Option<bool>,
    /// Vision support state:
    /// - Some(true) = model supports vision
    /// - Some(false) = model does not support vision (detected from error)
    /// - None = unknown (optimistic default)
    pub vision_supported: Option<bool>,
    /// Temperature from app config (wired through so build_config uses it)
    pub temperature: f32,
    /// Max tokens from app config
    pub max_tokens: usize,
    /// Ollama-specific hardware options from app config
    pub ollama_options: OllamaOptions,
}

impl ModelState {
    pub fn new(model: Box<dyn Model>, model_id: String) -> Self {
        let model_name = model.name().to_string();
        Self {
            model: Arc::new(RwLock::new(model)),
            model_id,
            model_name,
            thinking_enabled: Some(true),
            vision_supported: None,
            temperature: crate::constants::DEFAULT_TEMPERATURE,
            max_tokens: crate::constants::DEFAULT_MAX_TOKENS,
            ollama_options: OllamaOptions::default(),
        }
    }

    /// Get a reference to the model for reading
    pub fn model_ref(&self) -> &Arc<RwLock<Box<dyn Model>>> {
        &self.model
    }

    /// Toggle thinking mode (only if model supports it)
    /// Returns the new state, or None if model doesn't support thinking
    pub fn toggle_thinking(&mut self) -> Option<bool> {
        match self.thinking_enabled {
            Some(enabled) => {
                self.thinking_enabled = Some(!enabled);
                self.thinking_enabled
            },
            None => None, // Model doesn't support thinking, can't toggle
        }
    }

    /// Mark model as not supporting thinking
    /// Called when we get "does not support thinking" error from Ollama
    pub fn disable_thinking_support(&mut self) {
        self.thinking_enabled = None;
    }

    /// Check if thinking is currently active
    pub fn is_thinking_active(&self) -> bool {
        self.thinking_enabled == Some(true)
    }

    /// Build a ModelConfig for API calls using current model state
    pub fn build_config(&self) -> ModelConfig {
        let mut config = ModelConfig {
            model: self.model_id.clone(),
            thinking_enabled: self.thinking_enabled,
            temperature: self.temperature,
            max_tokens: self.max_tokens,
            ..ModelConfig::default()
        };
        // Wire Ollama-specific options from app config
        if let Some(v) = self.ollama_options.num_gpu {
            config.set_backend_option("ollama".into(), "num_gpu".into(), v.to_string());
        }
        if let Some(v) = self.ollama_options.num_ctx {
            config.set_backend_option("ollama".into(), "num_ctx".into(), v.to_string());
        }
        if let Some(v) = self.ollama_options.num_thread {
            config.set_backend_option("ollama".into(), "num_thread".into(), v.to_string());
        }
        if let Some(v) = self.ollama_options.numa {
            config.set_backend_option("ollama".into(), "numa".into(), v.to_string());
        }
        config
    }
}
