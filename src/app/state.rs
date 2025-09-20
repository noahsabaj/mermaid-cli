use std::sync::Arc;
use tokio::sync::RwLock;

use crate::app::Config;
use crate::models::{Model, ProjectContext};

/// Global application state
pub struct AppState {
    /// Configuration
    pub config: Arc<RwLock<Config>>,
    /// Current model
    pub model: Arc<RwLock<Box<dyn Model>>>,
    /// Project context
    pub context: Arc<RwLock<ProjectContext>>,
    /// Conversation history (for context)
    pub history: Arc<RwLock<Vec<(String, String)>>>,
}

impl AppState {
    /// Create new app state
    pub fn new(config: Config, model: Box<dyn Model>, context: ProjectContext) -> Self {
        Self {
            config: Arc::new(RwLock::new(config)),
            model: Arc::new(RwLock::new(model)),
            context: Arc::new(RwLock::new(context)),
            history: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Switch to a different model
    pub async fn switch_model(&self, new_model: Box<dyn Model>) {
        let mut model = self.model.write().await;
        *model = new_model;
    }

    /// Update configuration
    pub async fn update_config(&self, new_config: Config) {
        let mut config = self.config.write().await;
        *config = new_config;
    }

    /// Add to conversation history
    pub async fn add_to_history(&self, user_msg: String, assistant_msg: String) {
        let mut history = self.history.write().await;
        history.push((user_msg, assistant_msg));

        // Keep history manageable (last 10 exchanges)
        if history.len() > 10 {
            history.drain(0..1);
        }
    }

    /// Clear conversation history
    pub async fn clear_history(&self) {
        let mut history = self.history.write().await;
        history.clear();
    }
}