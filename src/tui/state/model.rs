/// Model state management
///
/// Handles LLM configuration and identity.

use std::sync::Arc;
use tokio::sync::RwLock;

use crate::models::Model;

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
}

impl ModelState {
    pub fn new(model: Box<dyn Model>, model_id: String) -> Self {
        let model_name = model.name().to_string();
        Self {
            model: Arc::new(RwLock::new(model)),
            model_id,
            model_name,
            // Default: thinking enabled (will be disabled if model doesn't support it)
            thinking_enabled: Some(true),
            // Default: vision unknown (optimistic — try until error)
            vision_supported: None,
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
            }
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
}
