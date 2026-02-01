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
}

impl ModelState {
    pub fn new(model: Box<dyn Model>, model_id: String) -> Self {
        let model_name = model.name().to_string();
        Self {
            model: Arc::new(RwLock::new(model)),
            model_id,
            model_name,
        }
    }

    /// Get a reference to the model for reading
    pub fn model_ref(&self) -> &Arc<RwLock<Box<dyn Model>>> {
        &self.model
    }
}
