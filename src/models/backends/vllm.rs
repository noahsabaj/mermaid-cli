/// vLLM backend implementation
///
/// Provides access to vLLM's OpenAI-compatible API.
/// Implements the ModelBackend trait for consistent interface with other backends.

use anyhow::{Context as _, Result};
use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::super::backend::ModelBackend;
use super::super::types::{ChatMessage, MessageRole, ModelConfig, ModelResponse, StreamCallback};
use crate::constants::HTTP_REQUEST_TIMEOUT_SECS;

/// vLLM model backend
pub struct VLLMBackend {
    client: Client,
    base_url: String,
}

impl std::fmt::Debug for VLLMBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VLLMBackend")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

/// Normalize vLLM URL for client connections
fn normalize_vllm_url(url: String) -> String {
    let mut normalized = url.trim().to_string();

    if !normalized.starts_with("http://") && !normalized.starts_with("https://") {
        normalized = format!("http://{}", normalized);
    }

    if !normalized.contains(':') {
        normalized = format!("{}:8000", normalized);
    }

    normalized
}

/// Verify vLLM is running by checking the models endpoint
async fn check_vllm_running(client: &Client, base_url: &str) -> Result<()> {
    let url = format!("{}/v1/models", base_url);
    client
        .get(&url)
        .send()
        .await
        .with_context(|| {
            format!(
                "vLLM not running at {}. Start with: python -m vllm.entrypoints.openai.api_server",
                base_url
            )
        })?;
    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
struct OpenAIModelsResponse {
    object: String,
    data: Vec<OpenAIModel>,
}

#[derive(Debug, Serialize, Deserialize)]
struct OpenAIModel {
    id: String,
    object: String,
    owned_by: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct OpenAIStreamChunk {
    id: String,
    object: String,
    created: u64,
    model: String,
    choices: Vec<OpenAIChoice>,
}

#[derive(Debug, Serialize, Deserialize)]
struct OpenAIChoice {
    index: u32,
    delta: OpenAIDelta,
    finish_reason: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct OpenAIDelta {
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    content: Option<String>,
}

impl VLLMBackend {
    /// Create a new vLLM backend instance
    pub async fn new(base_url: Option<&str>) -> Result<Self> {
        let base_url = base_url
            .map(|s| s.to_string())
            .or_else(|| std::env::var("VLLM_API_BASE").ok())
            .unwrap_or_else(|| "http://localhost:8000".to_string());

        let base_url = normalize_vllm_url(base_url);

        let client = Client::builder()
            .pool_max_idle_per_host(10)
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .timeout(std::time::Duration::from_secs(HTTP_REQUEST_TIMEOUT_SECS))
            .tcp_keepalive(std::time::Duration::from_secs(60))
            .build()
            .context("Failed to build HTTP client for vLLM")?;

        check_vllm_running(&client, &base_url).await?;

        Ok(Self { client, base_url })
    }
}

#[async_trait]
impl ModelBackend for VLLMBackend {
    async fn list_models(&self) -> Result<Vec<String>> {
        let url = format!("{}/v1/models", self.base_url);
        let response = self
            .client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("Failed to list models from vLLM at {}", self.base_url))?;

        if !response.status().is_success() {
            anyhow::bail!("vLLM API error listing models: {}", response.status());
        }

        let models_response: OpenAIModelsResponse = response
            .json()
            .await
            .context("Failed to parse vLLM models response")?;

        Ok(models_response.data.into_iter().map(|m| m.id).collect())
    }

    async fn health_check(&self) -> Result<()> {
        check_vllm_running(&self.client, &self.base_url).await
    }

    fn backend_name(&self) -> &str {
        "vLLM"
    }

    fn base_url(&self) -> &str {
        &self.base_url
    }

    async fn generate_stream(
        &self,
        model_name: &str,
        messages: &[ChatMessage],
        config: &ModelConfig,
        callback: StreamCallback,
    ) -> Result<ModelResponse> {
        let mut openai_messages = Vec::new();

        if let Some(system) = &config.system_prompt {
            openai_messages.push(json!({
                "role": "system",
                "content": system
            }));
        }

        for msg in messages {
            let role = match msg.role {
                MessageRole::User => "user",
                MessageRole::Assistant => "assistant",
                MessageRole::System => "system",
            };
            openai_messages.push(json!({
                "role": role,
                "content": msg.content
            }));
        }

        let mut request_body = json!({
            "model": model_name,
            "messages": openai_messages,
            "stream": true,
        });

        if let Some(temp) = config.temperature {
            request_body["temperature"] = json!(temp);
        }

        if let Some(max_tokens) = config.max_tokens {
            request_body["max_tokens"] = json!(max_tokens);
        }

        if let Some(top_p) = config.top_p {
            request_body["top_p"] = json!(top_p);
        }

        let url = format!("{}/v1/chat/completions", self.base_url);

        let response = self
            .client
            .post(&url)
            .json(&request_body)
            .send()
            .await
            .with_context(|| {
                format!(
                    "Failed to connect to vLLM at {}. Is vLLM running?",
                    self.base_url
                )
            })?;

        if !response.status().is_success() {
            let error_text = response.text().await?;
            anyhow::bail!("vLLM API error: {}", error_text);
        }

        let mut stream = response.bytes_stream();
        let mut full_response = String::new();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            let text = String::from_utf8_lossy(&chunk);

            for line in text.lines() {
                let line = line.trim();

                if line.is_empty() || line == "[DONE]" {
                    continue;
                }

                if !line.starts_with("data: ") {
                    continue;
                }

                let json_str = &line[6..];

                if json_str == "[DONE]" {
                    break;
                }

                if let Ok(chunk) = serde_json::from_str::<OpenAIStreamChunk>(json_str) {
                    if let Some(choice) = chunk.choices.first() {
                        if let Some(content) = &choice.delta.content {
                            if !content.is_empty() {
                                callback(content);
                                full_response.push_str(content);
                            }
                        }

                        if choice.finish_reason.is_some() {
                            break;
                        }
                    }
                }
            }
        }

        Ok(ModelResponse {
            content: full_response,
            usage: None,
            model_name: model_name.to_string(),
        })
    }
}
