/// vLLM backend adapter
///
/// Provides unified interface to vLLM's OpenAI-compatible API with connection pooling
/// and zero-unwrap error handling.

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

/// vLLM adapter with connection pooling
pub struct VLLMAdapter {
    client: Client,
    base_url: String,
    config: Arc<BackendConfig>,
}

impl VLLMAdapter {
    /// Create a new vLLM adapter
    pub async fn new(config: Arc<BackendConfig>) -> Result<Self> {
        let base_url = normalize_url(&config.vllm_url);

        // Build HTTP client with connection pooling
        let client = Client::builder()
            .pool_max_idle_per_host(config.max_idle_per_host)
            .pool_idle_timeout(Duration::from_secs(90))
            .timeout(Duration::from_secs(config.request_timeout_secs))
            .tcp_keepalive(Duration::from_secs(60))
            .build()
            .map_err(|e| ModelError::Backend(BackendError::ConnectionFailed {
                backend: "vllm".to_string(),
                url: base_url.clone(),
                reason: e.to_string(),
            }))?;

        Ok(Self {
            client,
            base_url,
            config,
        })
    }

    /// Handle streaming response (OpenAI SSE format)
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

        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result.map_err(|e| ModelError::StreamError(e.to_string()))?;

            let text = String::from_utf8_lossy(&chunk);

            // Parse SSE format (data: {...})
            for line in text.lines() {
                if !line.starts_with("data: ") {
                    continue;
                }

                let data = &line[6..];
                if data == "[DONE]" {
                    break;
                }

                let json_chunk: OpenAIStreamChunk = serde_json::from_str(data)
                    .map_err(|e| ModelError::ParseError {
                        message: format!("Failed to parse vLLM response: {}", e),
                        raw: Some(data.to_string()),
                    })?;

                if let Some(choice) = json_chunk.choices.get(0) {
                    if let Some(ref content) = choice.delta.content {
                        callback(content);
                        full_content.push_str(content);
                    }
                }
            }
        }

        Ok(ModelResponse {
            content: full_content,
            usage: None, // vLLM streaming doesn't provide usage stats
            model_name: "vllm".to_string(),
            thinking: None,
        })
    }
}

#[async_trait]
impl Backend for VLLMAdapter {
    fn name(&self) -> &str {
        "vllm"
    }

    async fn health_check(&self) -> Result<()> {
        let url = format!("{}/v1/models", self.base_url);

        let response = self.client
            .get(&url)
            .send()
            .await
            .map_err(|e| ModelError::Backend(BackendError::ConnectionFailed {
                backend: "vllm".to_string(),
                url: self.base_url.clone(),
                reason: format!("Health check failed: {}", e),
            }))?;

        if response.status().is_success() {
            Ok(())
        } else {
            Err(ModelError::Backend(BackendError::NotAvailable {
                backend: "vllm".to_string(),
                reason: format!("HTTP {}", response.status()),
            }))
        }
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        let url = format!("{}/v1/models", self.base_url);

        let response = self.client
            .get(&url)
            .send()
            .await
            .map_err(|e| ModelError::Backend(BackendError::ConnectionFailed {
                backend: "vllm".to_string(),
                url: self.base_url.clone(),
                reason: e.to_string(),
            }))?;

        if !response.status().is_success() {
            return Err(ModelError::Backend(BackendError::HttpError {
                status: response.status().as_u16(),
                message: "Failed to list models".to_string(),
            }));
        }

        let models_response: OpenAIModelsResponse = response.json().await
            .map_err(|e| ModelError::ParseError {
                message: format!("Failed to parse models response: {}", e),
                raw: None,
            })?;

        Ok(models_response.data.into_iter().map(|m| m.id).collect())
    }

    async fn chat(
        &self,
        model_name: &str,
        messages: &[ChatMessage],
        context: &ProjectContext,
        config: &ModelConfig,
        stream_callback: Option<StreamCallback>,
    ) -> Result<ModelResponse> {
        let url = format!("{}/v1/chat/completions", self.base_url);

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

        // Build request body (OpenAI format)
        let mut request_body = json!({
            "model": model_name,
            "messages": json_messages,
            "stream": stream_callback.is_some(),
        });

        // Add model parameters
        request_body["temperature"] = json!(config.temperature);
        if let Some(max_tokens) = Some(config.max_tokens) {
            request_body["max_tokens"] = json!(max_tokens);
        }
        if let Some(top_p) = config.top_p {
            request_body["top_p"] = json!(top_p);
        }

        // Send request
        let response = self.client
            .post(&url)
            .json(&request_body)
            .send()
            .await
            .map_err(|e| ModelError::Backend(BackendError::ConnectionFailed {
                backend: "vllm".to_string(),
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

            let json: OpenAIChatResponse = response.json().await
                .map_err(|e| ModelError::ParseError {
                    message: format!("Failed to parse response: {}", e),
                    raw: None,
                })?;

            let usage = json.usage.map(|u| TokenUsage {
                prompt_tokens: u.prompt_tokens,
                completion_tokens: u.completion_tokens,
                total_tokens: u.total_tokens,
            });

            let content = json.choices.get(0)
                .and_then(|c| Some(c.message.content.clone()))
                .unwrap_or_default();

            Ok(ModelResponse {
                content,
                usage,
                model_name: model_name.to_string(),
                thinking: None,
            })
        }
    }

    fn metadata(&self) -> BackendMetadata {
        BackendMetadata {
            max_context_length: 8192, // Default, varies by model
            supports_streaming: true,
            supports_functions: false,
            supports_vision: false,
            is_local: true,
            version: None,
        }
    }

    async fn shutdown(&self) -> Result<()> {
        // No persistent connections to clean up (handled by Drop)
        Ok(())
    }
}

// Response types (OpenAI-compatible)

#[derive(Debug, Serialize, Deserialize)]
struct OpenAIModelsResponse {
    data: Vec<OpenAIModel>,
}

#[derive(Debug, Serialize, Deserialize)]
struct OpenAIModel {
    id: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct OpenAIChatResponse {
    choices: Vec<OpenAIChoice>,
    usage: Option<OpenAIUsage>,
}

#[derive(Debug, Serialize, Deserialize)]
struct OpenAIChoice {
    message: OpenAIMessage,
}

#[derive(Debug, Serialize, Deserialize)]
struct OpenAIMessage {
    content: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct OpenAIUsage {
    prompt_tokens: usize,
    completion_tokens: usize,
    total_tokens: usize,
}

#[derive(Debug, Serialize, Deserialize)]
struct OpenAIStreamChunk {
    choices: Vec<OpenAIStreamChoice>,
}

#[derive(Debug, Serialize, Deserialize)]
struct OpenAIStreamChoice {
    delta: OpenAIDelta,
}

#[derive(Debug, Serialize, Deserialize)]
struct OpenAIDelta {
    #[serde(default)]
    content: Option<String>,
}

// Helper functions

fn normalize_url(url: &str) -> String {
    let mut normalized = url.trim().to_string();

    // Add http:// if missing
    if !normalized.starts_with("http://") && !normalized.starts_with("https://") {
        normalized = format!("http://{}", normalized);
    }

    // Add default port if missing
    if !normalized.contains(':') || normalized.matches(':').count() == 1 {
        if normalized.starts_with("http://") && !normalized[7..].contains(':') {
            normalized = format!("{}:8000", normalized);
        } else if normalized.starts_with("https://") && !normalized[8..].contains(':') {
            normalized = format!("{}:8000", normalized);
        }
    }

    normalized
}
