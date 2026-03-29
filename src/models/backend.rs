//! Model factory - creates model instances from identifiers
//!
//! Parses model identifiers like "ollama/llama3" and creates
//! the appropriate adapter implementing the Model trait.

use std::sync::Arc;
use std::time::Duration;

use super::config::BackendConfig;
use super::error::{ModelError, Result};
use super::traits::Model;

/// Model factory - creates model instances
pub struct ModelFactory {
    config: Arc<BackendConfig>,
}

impl ModelFactory {
    /// Create a new model factory
    pub fn new(config: BackendConfig) -> Self {
        Self {
            config: Arc::new(config),
        }
    }

    /// Create a model from a full identifier (e.g., "ollama/llama3")
    pub async fn create_model(&self, model_id: &str) -> Result<Box<dyn Model>> {
        // Parse model identifier: "provider/model_name" or just "model_name" (defaults to ollama)
        let (provider, model_name) = parse_model_id(model_id);

        match provider.to_lowercase().as_str() {
            "ollama" => {
                use super::adapters::ollama::OllamaAdapter;
                let adapter = OllamaAdapter::new(model_name, self.config.clone()).await?;
                Ok(Box::new(adapter))
            }
            _ => Err(ModelError::InvalidRequest(
                format!("Unknown provider: {}. Only ollama/ is supported.", provider),
            )),
        }
    }

    /// List available providers with a fast single-shot health check
    pub async fn available_providers(&self) -> Vec<String> {
        let mut providers = Vec::new();

        // Quick Ollama check: single GET with 2s timeout, no retries
        let url = format!("{}/api/tags", self.config.ollama_url.trim().trim_end_matches('/'));
        if let Ok(client) = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            && let Ok(resp) = client.get(&url).send().await
                && resp.status().is_success() {
                    providers.push("ollama".to_string());
                }

        providers
    }
}

/// Parse a model identifier into provider and model name
///
/// Formats:
/// - "ollama/llama3" -> ("ollama", "llama3")
/// - "llama3" -> ("ollama", "llama3")  // defaults to ollama
/// - "llama3:latest" -> ("ollama", "llama3:latest")  // ollama tag format
fn parse_model_id(model_id: &str) -> (&str, &str) {
    if let Some(idx) = model_id.find('/') {
        let provider = &model_id[..idx];
        let model = &model_id[idx + 1..];
        (provider, model)
    } else {
        // Default to ollama for bare model names
        ("ollama", model_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_model_id_with_provider() {
        let (provider, model) = parse_model_id("ollama/llama3");
        assert_eq!(provider, "ollama");
        assert_eq!(model, "llama3");
    }

    #[test]
    fn test_parse_model_id_bare_name() {
        let (provider, model) = parse_model_id("llama3");
        assert_eq!(provider, "ollama");
        assert_eq!(model, "llama3");
    }

    #[test]
    fn test_parse_model_id_with_tag() {
        let (provider, model) = parse_model_id("ollama/llama3:latest");
        assert_eq!(provider, "ollama");
        assert_eq!(model, "llama3:latest");
    }

    #[test]
    fn test_parse_model_id_bare_with_tag() {
        let (provider, model) = parse_model_id("llama3:7b");
        assert_eq!(provider, "ollama");
        assert_eq!(model, "llama3:7b");
    }
}
