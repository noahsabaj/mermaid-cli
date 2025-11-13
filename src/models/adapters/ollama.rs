/// Ollama backend adapter
///
/// Provides unified interface to Ollama (both local and cloud) with connection pooling,
/// health monitoring, and zero-unwrap error handling.

use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

use crate::models::backend::{Backend, BackendMetadata};
use crate::models::config::{BackendConfig, ModelConfig};
use crate::models::error::{BackendError, ModelError, Result};
use crate::models::types::{ChatMessage, MessageRole, ModelResponse, ProjectContext, StreamCallback, TokenUsage};

/// Ollama adapter with connection pooling
pub struct OllamaAdapter {
    client: Client,
    base_url: String,
    is_cloud: bool,
    cloud_api_key: Option<String>,
    config: Arc<BackendConfig>,
}

impl OllamaAdapter {
    /// Create a new Ollama adapter
    pub async fn new(config: Arc<BackendConfig>) -> Result<Self> {
        // Check for cloud API key (from env or future config)
        let cloud_api_key = std::env::var("OLLAMA_API_KEY").ok();
        let is_cloud = cloud_api_key.is_some();

        // Determine base URL
        let base_url = if is_cloud {
            std::env::var("OLLAMA_CLOUD_URL")
                .unwrap_or_else(|_| "https://api.ollama.com".to_string())
        } else {
            normalize_url(&config.ollama_url)
        };

        // Build HTTP client with connection pooling
        let client = Client::builder()
            .pool_max_idle_per_host(config.max_idle_per_host)
            .pool_idle_timeout(Duration::from_secs(90))
            .timeout(Duration::from_secs(config.request_timeout_secs))
            .tcp_keepalive(Duration::from_secs(60))
            .build()
            .map_err(|e| ModelError::Backend(BackendError::ConnectionFailed {
                backend: "ollama".to_string(),
                url: base_url.clone(),
                reason: e.to_string(),
            }))?;

        Ok(Self {
            client,
            base_url,
            is_cloud,
            cloud_api_key,
            config,
        })
    }

    /// Build request with optional cloud authentication
    fn build_request(&self, url: &str) -> reqwest::RequestBuilder {
        let mut req = self.client.post(url);
        if let Some(ref key) = self.cloud_api_key {
            req = req.header("Authorization", format!("Bearer {}", key));
        }
        req
    }

    /// Handle streaming response
    async fn handle_stream(
        &self,
        response: reqwest::Response,
        callback: StreamCallback,
    ) -> Result<ModelResponse> {
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let error_text = response.text().await.unwrap_or_else(|_| "Unknown error".to_string());
            return Err(ModelError::Backend(BackendError::HttpError {
                status,
                message: error_text,
            }));
        }

        let mut stream = response.bytes_stream();
        let mut full_content = String::new();
        let mut full_thinking = String::new();
        let mut in_thinking_phase = false;
        let mut prompt_tokens = 0;
        let mut completion_tokens = 0;

        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result.map_err(|e| ModelError::StreamError(e.to_string()))?;

            let text = String::from_utf8_lossy(&chunk);

            // Parse each line as JSON
            for line in text.lines() {
                if line.trim().is_empty() {
                    continue;
                }

                let json_chunk: OllamaStreamChunk = serde_json::from_str(line)
                    .map_err(|e| ModelError::ParseError {
                        message: format!("Failed to parse Ollama response: {}", e),
                        raw: Some(line.to_string()),
                    })?;

                // Handle thinking content (if present)
                if let Some(ref thinking_chunk) = json_chunk.message.thinking {
                    if !in_thinking_phase {
                        callback("Thinking...\n");
                        in_thinking_phase = true;
                    }
                    if !thinking_chunk.is_empty() {
                        callback(thinking_chunk);
                        full_thinking.push_str(thinking_chunk);
                    }
                }

                // Handle regular content
                if !json_chunk.message.content.is_empty() {
                    // Transition from thinking to answer
                    if in_thinking_phase {
                        callback("\n...done thinking.\n\n");
                        in_thinking_phase = false;
                    }
                    callback(&json_chunk.message.content);
                    full_content.push_str(&json_chunk.message.content);
                }

                // Capture token usage
                if json_chunk.done {
                    if let Some(count) = json_chunk.prompt_eval_count {
                        prompt_tokens = count;
                    }
                    if let Some(count) = json_chunk.eval_count {
                        completion_tokens = count;
                    }
                }
            }
        }

        let thinking = if full_thinking.is_empty() {
            None
        } else {
            Some(full_thinking)
        };

        Ok(ModelResponse {
            content: full_content,
            usage: Some(TokenUsage {
                prompt_tokens,
                completion_tokens,
                total_tokens: prompt_tokens + completion_tokens,
            }),
            model_name: "ollama".to_string(),
            thinking,
        })
    }
}

#[async_trait]
impl Backend for OllamaAdapter {
    fn name(&self) -> &str {
        if self.is_cloud {
            "ollama-cloud"
        } else {
            "ollama"
        }
    }

    async fn health_check(&self) -> Result<()> {
        let url = format!("{}/api/tags", self.base_url);

        let response = self.build_request(&url)
            .send()
            .await
            .map_err(|e| ModelError::Backend(BackendError::ConnectionFailed {
                backend: "ollama".to_string(),
                url: self.base_url.clone(),
                reason: format!("Health check failed: {}", e),
            }))?;

        if response.status().is_success() {
            Ok(())
        } else {
            Err(ModelError::Backend(BackendError::NotAvailable {
                backend: "ollama".to_string(),
                reason: format!("HTTP {}", response.status()),
            }))
        }
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        let url = format!("{}/api/tags", self.base_url);

        let response = self.build_request(&url)
            .send()
            .await
            .map_err(|e| ModelError::Backend(BackendError::ConnectionFailed {
                backend: "ollama".to_string(),
                url: self.base_url.clone(),
                reason: e.to_string(),
            }))?;

        if !response.status().is_success() {
            return Err(ModelError::Backend(BackendError::HttpError {
                status: response.status().as_u16(),
                message: "Failed to list models".to_string(),
            }));
        }

        let tags: OllamaTagsResponse = response.json().await
            .map_err(|e| ModelError::ParseError {
                message: format!("Failed to parse tags response: {}", e),
                raw: None,
            })?;

        Ok(tags.models.into_iter().map(|m| m.name).collect())
    }

    async fn chat(
        &self,
        model_name: &str,
        messages: &[ChatMessage],
        context: &ProjectContext,
        config: &ModelConfig,
        stream_callback: Option<StreamCallback>,
    ) -> Result<ModelResponse> {
        let url = format!("{}/api/chat", self.base_url);

        // Extract Ollama-specific options
        let ollama_opts = config.ollama_options();

        // Build messages array
        let mut json_messages = Vec::new();

        // Add system prompt if configured
        if let Some(ref system_prompt) = config.system_prompt {
            json_messages.push(json!({
                "role": "system",
                "content": system_prompt
            }));
        }

        // Add project context
        let context_str = context.to_prompt_context();
        if !context_str.is_empty() {
            json_messages.push(json!({
                "role": "system",
                "content": format!("Project Context:\n{}", context_str)
            }));
        }

        // Add conversation messages
        for msg in messages {
            let role = match msg.role {
                MessageRole::User => "user",
                MessageRole::Assistant => "assistant",
                MessageRole::System => "system",
            };
            json_messages.push(json!({
                "role": role,
                "content": msg.content
            }));
        }

        // Build request body
        let mut request_body = json!({
            "model": model_name,
            "messages": json_messages,
            "stream": stream_callback.is_some(),
        });

        // Add model parameters
        let mut options = json!({});
        if let Some(temp) = Some(config.temperature) {
            options["temperature"] = json!(temp);
        }
        if let Some(num_ctx) = ollama_opts.num_ctx {
            options["num_ctx"] = json!(num_ctx);
        }
        if let Some(num_gpu) = ollama_opts.num_gpu {
            options["num_gpu"] = json!(num_gpu);
        }
        if let Some(num_thread) = ollama_opts.num_thread {
            options["num_thread"] = json!(num_thread);
        }
        if let Some(numa) = ollama_opts.numa {
            options["numa"] = json!(numa);
        }

        if !options.as_object().map(|o| o.is_empty()).unwrap_or(true) {
            request_body["options"] = options;
        }

        // Send request
        let response = self.build_request(&url)
            .json(&request_body)
            .send()
            .await
            .map_err(|e| ModelError::Backend(BackendError::ConnectionFailed {
                backend: "ollama".to_string(),
                url: self.base_url.clone(),
                reason: e.to_string(),
            }))?;

        if let Some(callback) = stream_callback {
            self.handle_stream(response, callback).await
        } else {
            // Non-streaming response
            if !response.status().is_success() {
                let status = response.status().as_u16();
                let error_text = response.text().await.unwrap_or_else(|_| "Unknown error".to_string());
                return Err(ModelError::Backend(BackendError::HttpError {
                    status,
                    message: error_text,
                }));
            }

            let json: OllamaStreamChunk = response.json().await
                .map_err(|e| ModelError::ParseError {
                    message: format!("Failed to parse response: {}", e),
                    raw: None,
                })?;

            let thinking = json.message.thinking.filter(|t| !t.is_empty());

            Ok(ModelResponse {
                content: json.message.content,
                usage: None,
                model_name: model_name.to_string(),
                thinking,
            })
        }
    }

    fn metadata(&self) -> BackendMetadata {
        BackendMetadata {
            max_context_length: 8192, // Default, varies by model
            supports_streaming: true,
            supports_functions: false,
            supports_vision: false,
            is_local: !self.is_cloud,
            version: None,
        }
    }

    async fn shutdown(&self) -> Result<()> {
        // No persistent connections to clean up (handled by Drop)
        Ok(())
    }
}

// Response types

#[derive(Debug, Serialize, Deserialize)]
struct OllamaStreamChunk {
    message: OllamaMessage,
    done: bool,
    #[serde(default)]
    prompt_eval_count: Option<usize>,
    #[serde(default)]
    eval_count: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize)]
struct OllamaMessage {
    role: String,
    content: String,
    #[serde(default)]
    thinking: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct OllamaTagsResponse {
    models: Vec<OllamaModel>,
}

#[derive(Debug, Serialize, Deserialize)]
struct OllamaModel {
    name: String,
}

// Helper functions

fn normalize_url(url: &str) -> String {
    let mut normalized = url.trim().to_string();

    // Replace 0.0.0.0 with 127.0.0.1
    if normalized.contains("0.0.0.0") {
        normalized = normalized.replace("0.0.0.0", "127.0.0.1");
    }

    // Add http:// if missing
    if !normalized.starts_with("http://") && !normalized.starts_with("https://") {
        normalized = format!("http://{}", normalized);
    }

    // Add default port if missing
    if !normalized.contains(':') || normalized.matches(':').count() == 1 {
        if normalized.starts_with("http://") && !normalized[7..].contains(':') {
            normalized = format!("{}:11434", normalized);
        } else if normalized.starts_with("https://") && !normalized[8..].contains(':') {
            normalized = format!("{}:11434", normalized);
        }
    }

    normalized
}
