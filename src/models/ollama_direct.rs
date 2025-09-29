use anyhow::{Context as _, Result};
use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::traits::Model;
use super::types::{
    ChatMessage, MessageRole, ModelCapabilities, ModelConfig, ModelResponse, ProjectContext,
    StreamCallback,
};
use crate::constants::{HTTP_REQUEST_TIMEOUT_SECS, OLLAMA_DEFAULT_CONTEXT};

/// Direct Ollama model implementation (no proxy needed)
///
/// This model talks directly to the Ollama API at localhost:11434.
/// No LiteLLM proxy, no API keys, no .env file needed.
/// This is the tested and working path for Ollama models.
pub struct OllamaDirectModel {
    client: Client,
    model_name: String,
    base_url: String,
}

// Manual Debug implementation since reqwest::Client doesn't implement Debug
impl std::fmt::Debug for OllamaDirectModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OllamaDirectModel")
            .field("model_name", &self.model_name)
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

/// Normalize Ollama URL for client connections
///
/// Handles the edge case where OLLAMA_HOST is set to 0.0.0.0 (server bind address).
/// 0.0.0.0 is used by the Ollama server to listen on all interfaces, but clients
/// cannot connect TO 0.0.0.0 - they need an actual IP address like 127.0.0.1.
///
/// Transformations:
/// - `0.0.0.0:11434` -> `http://127.0.0.1:11434`
/// - `http://0.0.0.0:11434` -> `http://127.0.0.1:11434`
/// - `0.0.0.0` -> `http://127.0.0.1:11434`
/// - `localhost:11434` -> `http://localhost:11434` (add http prefix)
/// - `http://localhost:11434` -> unchanged
fn normalize_ollama_url(url: &str) -> String {
    let mut normalized = url.trim().to_string();

    // Replace 0.0.0.0 with 127.0.0.1 (bind address -> connect address)
    if normalized.contains("0.0.0.0") {
        normalized = normalized.replace("0.0.0.0", "127.0.0.1");
    }

    // Ensure http:// prefix
    if !normalized.starts_with("http://") && !normalized.starts_with("https://") {
        normalized = format!("http://{}", normalized);
    }

    // Add default port if missing
    if !normalized.contains(':',) || normalized.matches(':').count() == 1 {
        // Has protocol but no port
        if normalized.starts_with("http://") && !normalized[7..].contains(':') {
            normalized = format!("{}:11434", normalized);
        } else if normalized.starts_with("https://") && !normalized[8..].contains(':') {
            normalized = format!("{}:11434", normalized);
        }
    }

    normalized
}

impl OllamaDirectModel {
    /// Create a new direct Ollama model instance
    ///
    /// model_id format: "ollama/qwen3-coder:30b"
    /// Extracts model name: "qwen3-coder:30b"
    pub async fn new(model_id: &str) -> Result<Self> {
        // Extract model name from provider/model format
        let model_name = model_id
            .strip_prefix("ollama/")
            .ok_or_else(|| {
                anyhow::anyhow!("Invalid Ollama model format. Expected 'ollama/model-name'")
            })?
            .to_string();

        // Get Ollama base URL from environment or use default
        let base_url = std::env::var("OLLAMA_HOST")
            .unwrap_or_else(|_| "http://localhost:11434".to_string());

        // Normalize URL (handles 0.0.0.0 bind addresses, adds http:// prefix, etc.)
        let base_url = normalize_ollama_url(&base_url);

        // Create HTTP client with optimized settings
        let client = Client::builder()
            .pool_max_idle_per_host(10)
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .timeout(std::time::Duration::from_secs(HTTP_REQUEST_TIMEOUT_SECS))
            .tcp_keepalive(std::time::Duration::from_secs(60))
            .build()
            .context("Failed to build HTTP client for Ollama")?;

        // Verify Ollama is running
        check_ollama_running(&client, &base_url).await?;

        Ok(Self {
            client,
            model_name,
            base_url,
        })
    }

    /// Get capabilities for Ollama models
    fn get_capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            max_context_length: OLLAMA_DEFAULT_CONTEXT,
            supports_streaming: true,
            supports_functions: false, // Ollama doesn't support function calling yet
            supports_vision: false,    // Most Ollama models don't support vision
        }
    }
}

#[async_trait]
impl Model for OllamaDirectModel {
    async fn chat(
        &mut self,
        messages: &[ChatMessage],
        context: &ProjectContext,
        config: &ModelConfig,
        stream_callback: Option<StreamCallback>,
    ) -> Result<ModelResponse> {
        // Build Ollama-format messages array
        let mut ollama_messages = Vec::new();

        // Add system prompt if configured
        if let Some(system) = &config.system_prompt {
            ollama_messages.push(json!({
                "role": "system",
                "content": system
            }));
        }

        // Add project context as system message if not empty
        let context_str = context.to_prompt_context();
        if !context_str.is_empty() {
            ollama_messages.push(json!({
                "role": "system",
                "content": format!("Project Context:\n{}", context_str)
            }));
        }

        // Convert ChatMessage array to Ollama format
        for msg in messages {
            let role = match msg.role {
                MessageRole::User => "user",
                MessageRole::Assistant => "assistant",
                MessageRole::System => "system",
            };
            ollama_messages.push(json!({
                "role": role,
                "content": msg.content
            }));
        }

        // Prepare request body (Ollama format)
        let mut request_body = json!({
            "model": self.model_name,
            "messages": ollama_messages,
            "stream": stream_callback.is_some(),
        });

        // Add optional parameters from config
        if let Some(temp) = config.temperature {
            request_body["temperature"] = json!(temp);
        }
        // Ollama doesn't use max_tokens, but has num_predict
        if let Some(max_tokens) = config.max_tokens {
            request_body["options"] = json!({
                "num_predict": max_tokens
            });
        }
        if let Some(top_p) = config.top_p {
            if request_body.get("options").is_none() {
                request_body["options"] = json!({});
            }
            request_body["options"]["top_p"] = json!(top_p);
        }

        // Make request to Ollama API
        let url = format!("{}/api/chat", self.base_url);

        if let Some(callback) = stream_callback {
            // Streaming response
            let response = self
                .client
                .post(&url)
                .json(&request_body)
                .send()
                .await
                .with_context(|| {
                    format!(
                        "Failed to connect to Ollama at {}. Is Ollama running? Try: ollama serve",
                        self.base_url
                    )
                })?;

            if !response.status().is_success() {
                let error_text = response.text().await?;
                anyhow::bail!("Ollama API error: {}", error_text);
            }

            let mut stream = response.bytes_stream();

            while let Some(chunk) = stream.next().await {
                let chunk = chunk?;
                let text = String::from_utf8_lossy(&chunk);

                // Ollama streams JSON objects line by line (not SSE format)
                for line in text.lines() {
                    if line.is_empty() {
                        continue;
                    }

                    if let Ok(json_chunk) = serde_json::from_str::<OllamaStreamChunk>(line) {
                        let content = &json_chunk.message.content;
                        if !content.is_empty() {
                            callback(content);
                        }

                        // Check if done
                        if json_chunk.done {
                            break;
                        }
                    }
                }
            }

            Ok(ModelResponse {
                content: String::new(), // Content already sent via callback
                usage: None,            // Ollama doesn't provide detailed token usage in streaming
                model_name: self.model_name.clone(),
            })
        } else {
            // Non-streaming response
            let response = self
                .client
                .post(&url)
                .json(&request_body)
                .send()
                .await
                .with_context(|| {
                    format!(
                        "Failed to connect to Ollama at {}. Is Ollama running? Try: ollama serve",
                        self.base_url
                    )
                })?;

            if !response.status().is_success() {
                let error_text = response.text().await?;
                anyhow::bail!("Ollama API error: {}", error_text);
            }

            let response_json: OllamaChatResponse = response.json().await?;

            Ok(ModelResponse {
                content: response_json.message.content,
                usage: None, // Ollama doesn't provide detailed token usage
                model_name: self.model_name.clone(),
            })
        }
    }

    fn name(&self) -> &str {
        &self.model_name
    }

    fn is_local(&self) -> bool {
        true // Ollama is always local
    }

    fn capabilities(&self) -> ModelCapabilities {
        self.get_capabilities()
    }

    async fn validate_connection(&self) -> Result<bool> {
        check_ollama_running(&self.client, &self.base_url).await
    }
}

/// Check if Ollama is running and accessible
async fn check_ollama_running(client: &Client, base_url: &str) -> Result<bool> {
    let url = format!("{}/api/tags", base_url);

    match client.get(&url).send().await {
        Ok(response) if response.status().is_success() => Ok(true),
        Ok(response) => {
            anyhow::bail!(
                "Ollama is running but returned error: {}",
                response.status()
            )
        },
        Err(_) => {
            anyhow::bail!(
                "Cannot connect to Ollama at {}.\n\
                 \n\
                 Make sure Ollama is running:\n\
                 1. Install Ollama: curl -fsSL https://ollama.com/install.sh | sh\n\
                 2. Start Ollama: ollama serve\n\
                 3. Pull a model: ollama pull {}\n\
                 \n\
                 Or check if OLLAMA_HOST environment variable is set correctly.",
                base_url,
                "qwen3-coder:30b"
            )
        },
    }
}

/// Ollama chat response format
#[derive(Debug, Deserialize)]
struct OllamaChatResponse {
    message: OllamaMessage,
    #[allow(dead_code)]
    done: bool,
}

/// Ollama stream chunk format
#[derive(Debug, Deserialize)]
struct OllamaStreamChunk {
    message: OllamaMessage,
    done: bool,
}

/// Ollama message format
#[derive(Debug, Deserialize, Serialize)]
struct OllamaMessage {
    role: String,
    content: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_model_name_parsing() {
        // Test valid format
        let model_id = "ollama/qwen3-coder:30b";
        let model_name = model_id.strip_prefix("ollama/").unwrap();
        assert_eq!(model_name, "qwen3-coder:30b");

        // Test another format
        let model_id = "ollama/llama2:7b";
        let model_name = model_id.strip_prefix("ollama/").unwrap();
        assert_eq!(model_name, "llama2:7b");
    }

    #[test]
    fn test_normalize_ollama_url() {
        // Test 0.0.0.0 bind address transformation
        assert_eq!(
            normalize_ollama_url("0.0.0.0:11434"),
            "http://127.0.0.1:11434"
        );
        assert_eq!(
            normalize_ollama_url("http://0.0.0.0:11434"),
            "http://127.0.0.1:11434"
        );
        assert_eq!(
            normalize_ollama_url("0.0.0.0"),
            "http://127.0.0.1:11434"
        );

        // Test adding http:// prefix
        assert_eq!(
            normalize_ollama_url("localhost:11434"),
            "http://localhost:11434"
        );
        assert_eq!(
            normalize_ollama_url("127.0.0.1:11434"),
            "http://127.0.0.1:11434"
        );

        // Test already valid URLs
        assert_eq!(
            normalize_ollama_url("http://localhost:11434"),
            "http://localhost:11434"
        );
        assert_eq!(
            normalize_ollama_url("http://127.0.0.1:11434"),
            "http://127.0.0.1:11434"
        );

        // Test adding default port
        assert_eq!(
            normalize_ollama_url("localhost"),
            "http://localhost:11434"
        );
        assert_eq!(
            normalize_ollama_url("http://localhost"),
            "http://localhost:11434"
        );

        // Test custom ports preserved
        assert_eq!(
            normalize_ollama_url("localhost:8080"),
            "http://localhost:8080"
        );
        assert_eq!(
            normalize_ollama_url("0.0.0.0:8080"),
            "http://127.0.0.1:8080"
        );

        // Test whitespace trimming
        assert_eq!(
            normalize_ollama_url("  0.0.0.0:11434  "),
            "http://127.0.0.1:11434"
        );
    }

    #[tokio::test]
    async fn test_invalid_format() {
        // Should fail without "ollama/" prefix
        let result = OllamaDirectModel::new("qwen3-coder:30b").await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Invalid Ollama model format"));
    }

    #[tokio::test]
    #[ignore] // Requires Ollama running
    async fn test_ollama_connection() {
        // Only runs if Ollama is actually running
        let result = OllamaDirectModel::new("ollama/tinyllama").await;
        if result.is_ok() {
            let model = result.unwrap();
            assert_eq!(model.name(), "tinyllama");
            assert!(model.is_local());
        }
    }
}
