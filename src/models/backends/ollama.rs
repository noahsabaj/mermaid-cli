/// Ollama backend implementation
///
/// Provides direct access to Ollama's HTTP API at localhost:11434.
/// Implements the ModelBackend trait for consistent interface with other backends.

use anyhow::{Context as _, Result};
use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::super::backend::ModelBackend;
use super::super::types::{ChatMessage, MessageRole, ModelConfig, ModelResponse, StreamCallback, TokenUsage};
use crate::constants::HTTP_REQUEST_TIMEOUT_SECS;

/// Ollama model backend
pub struct OllamaBackend {
    client: Client,
    base_url: String,
    num_gpu: Option<i32>,
    num_thread: Option<i32>,
    num_ctx: Option<i32>,
    numa: Option<bool>,
}

impl std::fmt::Debug for OllamaBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OllamaBackend")
            .field("base_url", &self.base_url)
            .field("num_gpu", &self.num_gpu)
            .field("num_thread", &self.num_thread)
            .field("num_ctx", &self.num_ctx)
            .field("numa", &self.numa)
            .finish_non_exhaustive()
    }
}

/// Normalize Ollama URL for client connections
fn normalize_ollama_url(url: String) -> String {
    let mut normalized = url.trim().to_string();

    if normalized.contains("0.0.0.0") {
        normalized = normalized.replace("0.0.0.0", "127.0.0.1");
    }

    if !normalized.starts_with("http://") && !normalized.starts_with("https://") {
        normalized = format!("http://{}", normalized);
    }

    if !normalized.contains(':') || normalized.matches(':').count() == 1 {
        if normalized.starts_with("http://") && !normalized[7..].contains(':') {
            normalized = format!("{}:11434", normalized);
        } else if normalized.starts_with("https://") && !normalized[8..].contains(':') {
            normalized = format!("{}:11434", normalized);
        }
    }

    normalized
}

/// Verify Ollama is running by checking the tags endpoint
async fn check_ollama_running(client: &Client, base_url: &str) -> Result<()> {
    let url = format!("{}/api/tags", base_url);
    client
        .get(&url)
        .send()
        .await
        .with_context(|| {
            format!(
                "Ollama not running at {}. Start with: ollama serve",
                base_url
            )
        })?;
    Ok(())
}

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
}

#[derive(Debug, Serialize, Deserialize)]
struct OllamaTagsResponse {
    models: Vec<OllamaModel>,
}

#[derive(Debug, Serialize, Deserialize)]
struct OllamaModel {
    name: String,
}

impl OllamaBackend {
    /// Create a new Ollama backend instance
    pub async fn new(base_url: Option<&str>, config: Option<&crate::app::Config>) -> Result<Self> {
        let base_url = base_url
            .map(|s| s.to_string())
            .or_else(|| std::env::var("OLLAMA_HOST").ok())
            .unwrap_or_else(|| "http://localhost:11434".to_string());

        let base_url = normalize_ollama_url(base_url);

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

        let client = Client::builder()
            .pool_max_idle_per_host(10)
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .timeout(std::time::Duration::from_secs(HTTP_REQUEST_TIMEOUT_SECS))
            .tcp_keepalive(std::time::Duration::from_secs(60))
            .build()
            .context("Failed to build HTTP client for Ollama")?;

        check_ollama_running(&client, &base_url).await?;

        Ok(Self {
            client,
            base_url,
            num_gpu,
            num_thread,
            num_ctx,
            numa,
        })
    }
}

#[async_trait]
impl ModelBackend for OllamaBackend {
    async fn list_models(&self) -> Result<Vec<String>> {
        let url = format!("{}/api/tags", self.base_url);
        let response = self
            .client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("Failed to list models from Ollama at {}", self.base_url))?;

        if !response.status().is_success() {
            anyhow::bail!("Ollama API error listing models: {}", response.status());
        }

        let tags: OllamaTagsResponse = response
            .json()
            .await
            .context("Failed to parse Ollama tags response")?;

        Ok(tags.models.into_iter().map(|m| m.name).collect())
    }

    async fn health_check(&self) -> Result<()> {
        check_ollama_running(&self.client, &self.base_url).await
    }

    fn backend_name(&self) -> &str {
        "Ollama"
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
        let mut ollama_messages = Vec::new();

        if let Some(system) = &config.system_prompt {
            ollama_messages.push(json!({
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
            ollama_messages.push(json!({
                "role": role,
                "content": msg.content
            }));
        }

        let mut request_body = json!({
            "model": model_name,
            "messages": ollama_messages,
            "stream": true,
        });

        if let Some(temp) = config.temperature {
            request_body["temperature"] = json!(temp);
        }

        let mut options = serde_json::Map::new();

        if let Some(max_tokens) = config.max_tokens {
            options.insert("num_predict".to_string(), json!(max_tokens));
        }
        if let Some(top_p) = config.top_p {
            options.insert("top_p".to_string(), json!(top_p));
        }

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

        if !options.is_empty() {
            request_body["options"] = json!(options);
        }

        let url = format!("{}/api/chat", self.base_url);

        let response = self
            .client
            .post(&url)
            .json(&request_body)
            .send()
            .await
            .with_context(|| {
                format!(
                    "Failed to connect to Ollama at {}. Is Ollama running?",
                    self.base_url
                )
            })?;

        if !response.status().is_success() {
            let error_text = response.text().await?;
            anyhow::bail!("Ollama API error: {}", error_text);
        }

        let mut stream = response.bytes_stream();
        let mut final_usage = None;
        let mut full_response = String::new();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            let text = String::from_utf8_lossy(&chunk);

            for line in text.lines() {
                if line.is_empty() {
                    continue;
                }

                if let Ok(json_chunk) = serde_json::from_str::<OllamaStreamChunk>(line) {
                    let content = &json_chunk.message.content;
                    if !content.is_empty() {
                        callback(content);
                        full_response.push_str(content);
                    }

                    if json_chunk.done {
                        let prompt_tokens = json_chunk.prompt_eval_count.unwrap_or(0);
                        let completion_tokens = json_chunk.eval_count.unwrap_or(0);
                        let total_tokens = prompt_tokens + completion_tokens;

                        if prompt_tokens > 0 || completion_tokens > 0 {
                            final_usage = Some(TokenUsage {
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
            content: full_response,
            usage: final_usage,
            model_name: model_name.to_string(),
        })
    }
}
