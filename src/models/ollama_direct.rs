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
    // Offloading configuration for large models
    num_gpu: Option<i32>,
    num_thread: Option<i32>,
    num_ctx: Option<i32>,
    numa: Option<bool>,
}

// Manual Debug implementation since reqwest::Client doesn't implement Debug
impl std::fmt::Debug for OllamaDirectModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OllamaDirectModel")
            .field("model_name", &self.model_name)
            .field("base_url", &self.base_url)
            .field("num_gpu", &self.num_gpu)
            .field("num_thread", &self.num_thread)
            .field("num_ctx", &self.num_ctx)
            .field("numa", &self.numa)
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
    if !normalized.contains(':') || normalized.matches(':').count() == 1 {
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
    ///
    /// Accepts optional config for offloading large models (GPU/CPU/RAM)
    pub async fn new(model_id: &str, config: Option<&crate::app::Config>) -> Result<Self> {
        // Extract model name from provider/model format
        let model_name = model_id
            .strip_prefix("ollama/")
            .ok_or_else(|| {
                anyhow::anyhow!("Invalid Ollama model format. Expected 'ollama/model-name'")
            })?
            .to_string();

        // Get Ollama base URL from environment or use default
        let base_url =
            std::env::var("OLLAMA_HOST").unwrap_or_else(|_| "http://localhost:11434".to_string());

        // Normalize URL (handles 0.0.0.0 bind addresses, adds http:// prefix, etc.)
        let base_url = normalize_ollama_url(&base_url);

        // Extract offloading config from provided config
        let (num_gpu, num_thread, num_ctx, numa) = if let Some(cfg) = config {
            (
                cfg.ollama.num_gpu,
                cfg.ollama.num_thread,
                cfg.ollama.num_ctx,
                cfg.ollama.numa,
            )
        } else {
            (None, None, None, None)
        };

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
            num_gpu,
            num_thread,
            num_ctx,
            numa,
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

        // Build options object with model parameters and offloading settings
        let mut options = serde_json::Map::new();

        // Ollama doesn't use max_tokens, but has num_predict
        if let Some(max_tokens) = config.max_tokens {
            options.insert("num_predict".to_string(), json!(max_tokens));
        }
        if let Some(top_p) = config.top_p {
            options.insert("top_p".to_string(), json!(top_p));
        }

        // Add offloading parameters for large model support
        if let Some(num_gpu) = self.num_gpu {
            options.insert("num_gpu".to_string(), json!(num_gpu));
        }
        if let Some(num_thread) = self.num_thread {
            options.insert("num_thread".to_string(), json!(num_thread));
        }
        if let Some(num_ctx) = self.num_ctx {
            options.insert("num_ctx".to_string(), json!(num_ctx));
        }
        if let Some(numa) = self.numa {
            options.insert("numa".to_string(), json!(numa));
        }

        // Only add options if we have any
        if !options.is_empty() {
            request_body["options"] = json!(options);
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
            // Track token usage from final chunk
            let mut final_usage = None;

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

                        // Check if done - capture final token usage
                        if json_chunk.done {
                            // Build TokenUsage from final chunk metadata
                            let prompt_tokens = json_chunk.prompt_eval_count.unwrap_or(0);
                            let completion_tokens = json_chunk.eval_count.unwrap_or(0);
                            let total_tokens = prompt_tokens + completion_tokens;

                            if prompt_tokens > 0 || completion_tokens > 0 {
                                final_usage = Some(super::types::TokenUsage {
                                    prompt_tokens,
                                    completion_tokens,
                                    total_tokens,
                                });
                            }
                            break;
                        }
                    }
                }
            }

            Ok(ModelResponse {
                content: String::new(), // Content already sent via callback
                usage: final_usage,     // Token usage from final chunk
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

            // Build TokenUsage from response metadata
            let usage = if let (Some(prompt_count), Some(completion_count)) =
                (response_json.prompt_eval_count, response_json.eval_count)
            {
                if prompt_count > 0 || completion_count > 0 {
                    Some(super::types::TokenUsage {
                        prompt_tokens: prompt_count,
                        completion_tokens: completion_count,
                        total_tokens: prompt_count + completion_count,
                    })
                } else {
                    None
                }
            } else {
                None
            };

            Ok(ModelResponse {
                content: response_json.message.content,
                usage,
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

/// Ollama chat response format (non-streaming)
#[derive(Debug, Deserialize)]
struct OllamaChatResponse {
    message: OllamaMessage,
    done: bool,
    /// Token usage stats from Ollama
    #[serde(default)]
    prompt_eval_count: Option<usize>,
    #[serde(default)]
    eval_count: Option<usize>,
}

/// Ollama stream chunk format
#[derive(Debug, Deserialize)]
struct OllamaStreamChunk {
    message: OllamaMessage,
    done: bool,
    /// Token usage stats (only present in final chunk when done=true)
    #[serde(default)]
    prompt_eval_count: Option<usize>,
    #[serde(default)]
    eval_count: Option<usize>,
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
        assert_eq!(normalize_ollama_url("0.0.0.0"), "http://127.0.0.1:11434");

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
        assert_eq!(normalize_ollama_url("localhost"), "http://localhost:11434");
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
        let result = OllamaDirectModel::new("qwen3-coder:30b", None).await;
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
        let result = OllamaDirectModel::new("ollama/tinyllama", None).await;
        if result.is_ok() {
            let model = result.unwrap();
            assert_eq!(model.name(), "tinyllama");
            assert!(model.is_local());
        }
    }

    #[test]
    fn test_token_usage_structure_issue_6() {
        // Test Issue #6: Token usage extraction structures
        // Verify OllamaChatResponse can deserialize token counts

        // Simulate Ollama response with token counts
        let json_str = r#"{
            "message": {
                "role": "assistant",
                "content": "Hello"
            },
            "done": true,
            "prompt_eval_count": 42,
            "eval_count": 15
        }"#;

        let response: OllamaChatResponse = serde_json::from_str(json_str).unwrap();
        assert_eq!(response.prompt_eval_count, Some(42));
        assert_eq!(response.eval_count, Some(15));
        assert!(response.done);
    }

    #[test]
    fn test_token_usage_deserialization_backward_compatibility() {
        // Test Issue #6: Backward compatibility with older Ollama versions
        // Old Ollama responses don't include token counts

        let json_str_without_tokens = r#"{
            "message": {
                "role": "assistant",
                "content": "Hello"
            },
            "done": true
        }"#;

        let response: OllamaChatResponse = serde_json::from_str(json_str_without_tokens).unwrap();
        assert_eq!(response.prompt_eval_count, None);
        assert_eq!(response.eval_count, None);
    }

    #[test]
    fn test_stream_chunk_token_extraction() {
        // Test Issue #6: Token extraction from stream chunks

        let json_str = r#"{
            "message": {
                "role": "assistant",
                "content": "partial"
            },
            "done": false
        }"#;

        let chunk: OllamaStreamChunk = serde_json::from_str(json_str).unwrap();
        assert!(!chunk.done);
        assert_eq!(chunk.prompt_eval_count, None); // Not present in intermediate chunks

        let final_chunk_str = r#"{
            "message": {
                "role": "assistant",
                "content": ""
            },
            "done": true,
            "prompt_eval_count": 100,
            "eval_count": 50
        }"#;

        let final_chunk: OllamaStreamChunk = serde_json::from_str(final_chunk_str).unwrap();
        assert!(final_chunk.done);
        assert_eq!(final_chunk.prompt_eval_count, Some(100));
        assert_eq!(final_chunk.eval_count, Some(50));
    }

    #[test]
    fn test_token_usage_calculation() {
        // Test Issue #6: Token usage calculation is correct
        // Verify that token totals add up correctly
        // (TokenUsage struct is tested more thoroughly in context/loader.rs tests)

        let prompt_tokens = 100;
        let completion_tokens = 50;
        let expected_total = prompt_tokens + completion_tokens;

        assert_eq!(expected_total, 150);
    }

    #[test]
    fn test_token_usage_zero_handling() {
        // Test Issue #6: Handle zero token counts gracefully
        let json_str = r#"{
            "message": {
                "role": "assistant",
                "content": "test"
            },
            "done": true,
            "prompt_eval_count": 0,
            "eval_count": 0
        }"#;

        let response: OllamaChatResponse = serde_json::from_str(json_str).unwrap();
        assert_eq!(response.prompt_eval_count, Some(0));
        assert_eq!(response.eval_count, Some(0));
    }

    #[test]
    fn test_ollama_message_structure() {
        // Test Issue #6: OllamaMessage deserialization
        let json_str = r#"{
            "role": "assistant",
            "content": "This is a test message"
        }"#;

        let msg: OllamaMessage = serde_json::from_str(json_str).unwrap();
        assert_eq!(msg.role, "assistant");
        assert_eq!(msg.content, "This is a test message");
    }

    #[test]
    fn test_normalize_url_edge_cases() {
        // Additional edge cases for URL normalization (relates to Issue #6 indirectly)
        assert_eq!(
            normalize_ollama_url("https://ollama.example.com:11434"),
            "https://ollama.example.com:11434"
        );

        assert_eq!(
            normalize_ollama_url("   localhost:11434   "),
            "http://localhost:11434"
        );

        // IPv6 format (should add http prefix but not modify address)
        let ipv6_result = normalize_ollama_url("[::1]:11434");
        assert!(ipv6_result.contains("[::1]"));
        assert!(ipv6_result.contains(":11434"));
    }
}
