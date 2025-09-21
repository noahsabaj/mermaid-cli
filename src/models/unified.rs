use anyhow::{Context as _, Result};
use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;
use futures::StreamExt;

use super::traits::Model;
use super::types::{ChatMessage, MessageRole, ModelCapabilities, ModelConfig, ModelResponse, ProjectContext, StreamCallback};
use crate::constants::{
    DEFAULT_LITELLM_PROXY_URL, HTTP_REQUEST_TIMEOUT_SECS,
    GPT4_32K_CONTEXT, GPT4_TURBO_CONTEXT, GPT35_CONTEXT,
    CLAUDE_3_OPUS_CONTEXT, CLAUDE_25_CONTEXT,
    OLLAMA_DEFAULT_CONTEXT, GROQ_LLAMA_CONTEXT, GROQ_DEFAULT_CONTEXT,
    GEMINI_15_PRO_CONTEXT,
};

/// Unified model implementation using LiteLLM Proxy
/// This drastically simplifies our code - ALL providers go through the same interface
pub struct UnifiedModel {
    client: Client,
    proxy_url: String,
    model_name: String,
    master_key: Option<String>,
}

impl UnifiedModel {
    /// Create a new unified model instance
    pub async fn new(model_name: &str, config_master_key: Option<String>) -> Result<Self> {
        // Get proxy URL from environment or use default
        let proxy_url = std::env::var("LITELLM_PROXY_URL")
            .unwrap_or_else(|_| DEFAULT_LITELLM_PROXY_URL.to_string());

        // Get master key for authentication
        // Priority: Environment variable > Config > None
        let master_key = std::env::var("LITELLM_MASTER_KEY")
            .ok()
            .or(config_master_key);

        Ok(Self {
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(HTTP_REQUEST_TIMEOUT_SECS))
                .build()?,
            proxy_url,
            model_name: model_name.to_string(),
            master_key,
        })
    }

    /// Get capabilities based on model name
    /// LiteLLM handles all the provider-specific details
    fn get_capabilities(&self) -> ModelCapabilities {
        // Parse provider from model name (e.g., "openai/gpt-4" -> "openai")
        let provider = self.model_name.split('/').next().unwrap_or("");
        let model = self.model_name.split('/').nth(1).unwrap_or(&self.model_name);

        match provider {
            "openai" => ModelCapabilities {
                max_context_length: if model.contains("gpt-4") {
                    if model.contains("32k") {
                        GPT4_32K_CONTEXT
                    } else {
                        GPT4_TURBO_CONTEXT
                    }
                } else {
                    GPT35_CONTEXT
                },
                supports_streaming: true,
                supports_functions: true,
                supports_vision: model.contains("vision") || model.contains("4o"),
            },
            "anthropic" => ModelCapabilities {
                max_context_length: if model.contains("claude-3") {
                    CLAUDE_3_OPUS_CONTEXT
                } else {
                    CLAUDE_25_CONTEXT
                },
                supports_streaming: true,
                supports_functions: true,
                supports_vision: model.contains("claude-3"),
            },
            "ollama" => ModelCapabilities {
                max_context_length: OLLAMA_DEFAULT_CONTEXT,
                supports_streaming: true,
                supports_functions: false,
                supports_vision: model.contains("llava") || model.contains("vision"),
            },
            "groq" => ModelCapabilities {
                max_context_length: if model.contains("mixtral") {
                    GROQ_LLAMA_CONTEXT
                } else {
                    GROQ_DEFAULT_CONTEXT
                },
                supports_streaming: true,
                supports_functions: false,
                supports_vision: false,
            },
            "google" | "gemini" => ModelCapabilities {
                max_context_length: GEMINI_15_PRO_CONTEXT,
                supports_streaming: true,
                supports_functions: true,
                supports_vision: model.contains("vision") || model.contains("pro"),
            },
            _ => ModelCapabilities::default(),
        }
    }

    /// Check if this model uses a local provider
    fn is_local_provider(&self) -> bool {
        self.model_name.starts_with("ollama/") ||
        self.model_name.starts_with("local/") ||
        self.model_name.starts_with("llamafile/")
    }
}

#[async_trait]
impl Model for UnifiedModel {
    async fn chat(
        &mut self,
        messages: &[ChatMessage],
        context: &ProjectContext,
        config: &ModelConfig,
        stream_callback: Option<StreamCallback>,
    ) -> Result<ModelResponse> {
        // Build OpenAI-compatible messages array
        let mut json_messages = Vec::new();

        // Add system prompt if configured
        if let Some(system) = &config.system_prompt {
            json_messages.push(json!({
                "role": "system",
                "content": system
            }));
        }

        // Add project context as system message if not empty
        let context_str = context.to_prompt_context();
        if !context_str.is_empty() {
            json_messages.push(json!({
                "role": "system",
                "content": format!("Project Context:\n{}", context_str)
            }));
        }

        // Convert ChatMessage array to JSON format
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

        // Prepare request body (OpenAI format - LiteLLM handles translation)
        let mut request_body = json!({
            "model": self.model_name,
            "messages": json_messages,
            "stream": stream_callback.is_some(),
        });

        // Add optional parameters from config
        if let Some(temp) = config.temperature {
            request_body["temperature"] = json!(temp);
        }
        if let Some(max_tokens) = config.max_tokens {
            request_body["max_tokens"] = json!(max_tokens);
        }
        if let Some(top_p) = config.top_p {
            request_body["top_p"] = json!(top_p);
        }

        // Make request to LiteLLM proxy
        let url = format!("{}/v1/chat/completions", self.proxy_url);

        if let Some(callback) = stream_callback {
            // Streaming response
            let mut request = self.client.post(&url).json(&request_body);

            // Add authentication header if master key is available
            if let Some(key) = &self.master_key {
                request = request.header("Authorization", format!("Bearer {}", key));
            }

            let response = request
                .send()
                .await
                .with_context(|| format!("Failed to connect to LiteLLM proxy at {}. Is the proxy running? Try: ./start_litellm.sh", self.proxy_url))?;

            if !response.status().is_success() {
                let error_text = response.text().await?;
                anyhow::bail!("LiteLLM proxy error: {}", error_text);
            }

            let mut stream = response.bytes_stream();
            let mut full_response = String::new();

            while let Some(chunk) = stream.next().await {
                let chunk = chunk?;
                let text = String::from_utf8_lossy(&chunk);

                // Parse SSE format
                for line in text.lines() {
                    if line.starts_with("data: ") {
                        let data = &line[6..];
                        if data == "[DONE]" {
                            break;
                        }

                        if let Ok(json_chunk) = serde_json::from_str::<StreamChunk>(data) {
                            if let Some(delta) = json_chunk.choices.get(0)
                                .and_then(|c| c.delta.content.as_ref())
                            {
                                full_response.push_str(delta);
                                callback(delta);
                            }
                        }
                    }
                }
            }

            Ok(ModelResponse {
                content: String::new(),  // Content already sent via callback, don't duplicate
                usage: None,  // Usage stats not available in streaming
                model_name: self.model_name.clone(),
            })
        } else {
            // Non-streaming response
            let mut request = self.client.post(&url).json(&request_body);

            // Add authentication header if master key is available
            if let Some(key) = &self.master_key {
                request = request.header("Authorization", format!("Bearer {}", key));
            }

            let response = request
                .send()
                .await
                .with_context(|| format!("Failed to connect to LiteLLM proxy at {}. Is the proxy running? Try: ./start_litellm.sh", self.proxy_url))?;

            if !response.status().is_success() {
                let error_text = response.text().await?;
                anyhow::bail!("LiteLLM proxy error: {}", error_text);
            }

            let response_json: ChatCompletionResponse = response.json().await?;

            Ok(ModelResponse {
                content: response_json.choices[0].message.content.clone(),
                usage: response_json.usage.map(|u| super::types::TokenUsage {
                    prompt_tokens: u.prompt_tokens,
                    completion_tokens: u.completion_tokens,
                    total_tokens: u.total_tokens,
                }),
                model_name: self.model_name.clone(),
            })
        }
    }

    fn name(&self) -> &str {
        &self.model_name
    }

    fn is_local(&self) -> bool {
        self.is_local_provider()
    }

    fn capabilities(&self) -> ModelCapabilities {
        self.get_capabilities()
    }

    async fn validate_connection(&self) -> Result<bool> {
        // Skip validation for local models to speed up startup
        if self.is_local_provider() {
            return Ok(true);
        }

        // Check if LiteLLM proxy is accessible with a short timeout
        let health_url = format!("{}/health", self.proxy_url);

        // Create a client with shorter timeout for health checks
        let health_client = Client::builder()
            .timeout(std::time::Duration::from_secs(3)) // 3 second timeout for health checks
            .build()?;

        let mut request = health_client.get(&health_url);
        if let Some(key) = &self.master_key {
            request = request.header("Authorization", format!("Bearer {}", key));
        }

        match request.send().await {
            Ok(response) => Ok(response.status().is_success()),
            Err(_) => {
                // Try alternate health check with /models endpoint
                let models_url = format!("{}/models", self.proxy_url);
                let mut request = health_client.get(&models_url);
                if let Some(key) = &self.master_key {
                    request = request.header("Authorization", format!("Bearer {}", key));
                }
                match request.send().await {
                    Ok(response) => Ok(response.status().is_success()),
                    Err(_) => Ok(false),
                }
            }
        }
    }
}

/// Helper function to create a unified model from a model string
pub async fn create_from_string(model_string: &str) -> Result<UnifiedModel> {
    UnifiedModel::new(model_string, None).await
}

// Response structures for LiteLLM proxy (OpenAI format)

#[derive(Debug, Deserialize)]
struct ChatCompletionResponse {
    choices: Vec<Choice>,
    usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: Message,
}

#[derive(Debug, Deserialize)]
struct Message {
    content: String,
}

#[derive(Debug, Deserialize)]
struct Usage {
    prompt_tokens: usize,
    completion_tokens: usize,
    total_tokens: usize,
}

#[derive(Debug, Deserialize)]
struct StreamChunk {
    choices: Vec<StreamChoice>,
}

#[derive(Debug, Deserialize)]
struct StreamChoice {
    delta: Delta,
}

#[derive(Debug, Deserialize)]
struct Delta {
    content: Option<String>,
}