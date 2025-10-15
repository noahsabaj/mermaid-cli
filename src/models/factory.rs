use anyhow::Result;

use super::ollama_direct::OllamaDirectModel;
use super::traits::Model;
use super::unified::UnifiedModel;
use crate::app::Config;

/// Factory for creating model instances
///
/// Routes to appropriate implementation based on provider:
/// - "ollama/*" -> Direct Ollama connection (no proxy needed)
/// - Other providers -> LiteLLM proxy (requires proxy running)
pub struct ModelFactory;

impl ModelFactory {
    /// Create a model instance from a model identifier with optional config
    ///
    /// Format: provider/model (e.g., "ollama/qwen3-coder:30b", "openai/gpt-4", "anthropic/claude-3-opus")
    ///
    /// Routing logic:
    /// - Ollama models: Direct connection to localhost:11434 (no proxy, no API keys)
    /// - API models: LiteLLM proxy handles authentication and routing
    pub async fn create(model_id: &str, config: Option<&Config>) -> Result<Box<dyn Model>> {
        // Validate format (provider/model)
        if !model_id.contains('/') {
            anyhow::bail!("Invalid model format. Expected 'provider/model' (e.g., 'ollama/qwen3-coder:30b')");
        }

        // Route based on provider
        if model_id.starts_with("ollama/") {
            // DIRECT CONNECTION - No proxy needed
            // This is the tested and working path
            // Pass config for offloading settings (num_gpu, num_thread, etc.)
            let model = OllamaDirectModel::new(model_id, config).await?;
            Ok(Box::new(model))
        } else {
            // API MODELS - Use LiteLLM proxy
            // Extract master_key from config if provided
            let master_key = config.and_then(|c| c.litellm.master_key.clone());
            let model = UnifiedModel::new(model_id, master_key).await?;
            Ok(Box::new(model))
        }
    }

    /// List available models from LiteLLM proxy
    pub async fn list_available() -> Result<Vec<String>> {
        use reqwest::Client;
        use serde::Deserialize;

        #[derive(Deserialize)]
        struct ModelsResponse {
            data: Vec<ModelInfo>,
        }

        #[derive(Deserialize)]
        struct ModelInfo {
            id: String,
        }

        // Get proxy URL from environment or use default
        let proxy_url = std::env::var("LITELLM_PROXY_URL")
            .unwrap_or_else(|_| "http://localhost:4000".to_string());

        // Get master key for authentication
        let master_key = std::env::var("LITELLM_MASTER_KEY").ok();

        // Query LiteLLM proxy for available models
        let client = Client::new();
        let url = format!("{}/v1/models", proxy_url);

        let mut request = client.get(&url);
        if let Some(key) = master_key {
            request = request.header("Authorization", format!("Bearer {}", key));
        }

        match request.send().await {
            Ok(response) if response.status().is_success() => {
                let models_response: ModelsResponse = response.json().await?;
                Ok(models_response.data.into_iter().map(|m| m.id).collect())
            },
            _ => {
                // Fallback to common models if proxy is not available
                Ok(vec![
                    "ollama/tinyllama".to_string(),
                    "ollama/llama2".to_string(),
                    "ollama/mistral".to_string(),
                    "ollama/codellama".to_string(),
                    "openai/gpt-4o".to_string(),
                    "openai/gpt-3.5-turbo".to_string(),
                    "anthropic/claude-3-sonnet".to_string(),
                    "groq/llama3-70b".to_string(),
                ])
            },
        }
    }

    /// Validate that a model is accessible
    pub async fn validate(model_id: &str, config: Option<&Config>) -> Result<bool> {
        match Self::create(model_id, config).await {
            Ok(model) => model.validate_connection().await,
            Err(_) => Ok(false),
        }
    }
}
