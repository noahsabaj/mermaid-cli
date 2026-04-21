//! OpenAI-compatible provider — C4 wrapper over the v0.6
//! `OpenAICompatAdapter`.
//!
//! This provider covers the OpenAI long-tail: OpenRouter, Groq,
//! Fireworks, Together, custom vLLM endpoints, plus the user-defined
//! entries in `[providers.*]`. The v0.6 adapter looks up a
//! `ProviderProfile` (registry entry) and applies per-provider
//! reasoning shapes (flat `reasoning_effort` vs nested `reasoning:
//! {effort}`). The wrapper just forwards.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::domain::ChatRequest;
use crate::models::adapters::openai_compat::OpenAICompatAdapter;
use crate::models::{
    Model, ModelConfig, ModelError, ProviderProfile, ReasoningChunk, Result, StreamCallback,
    StreamEvent as ModelStreamEvent,
};

use super::super::capabilities::Capabilities;
use super::super::ctx::{FinalResponse, StreamContext, StreamEvent};
use super::ModelProvider;

pub struct OpenAICompatProvider {
    adapter: OpenAICompatAdapter,
    capabilities: Capabilities,
}

impl OpenAICompatProvider {
    pub fn new(
        profile: &'static ProviderProfile,
        base_url: String,
        api_key: String,
        model_name: String,
        extra_headers: HashMap<String, String>,
    ) -> Result<Self> {
        let adapter =
            OpenAICompatAdapter::new(profile, base_url, api_key, model_name, extra_headers)?;
        let capabilities = Capabilities::from_legacy(adapter.capabilities());
        Ok(Self {
            adapter,
            capabilities,
        })
    }
}

#[async_trait]
impl ModelProvider for OpenAICompatProvider {
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    async fn chat(&self, request: ChatRequest, ctx: StreamContext) -> Result<FinalResponse> {
        let config = build_model_config(&request);
        let callback = forward_callback(ctx.sink.clone());
        let chat_fut = self
            .adapter
            .chat(&request.messages, &config, Some(callback));

        let response = tokio::select! {
            biased;
            _ = ctx.token.cancelled() => {
                let _ = ctx.sink.send(StreamEvent::Done {
                    usage: None,
                    thinking_signature: None,
                }).await;
                return Err(ModelError::StreamError("cancelled by user".to_string()));
            },
            r = chat_fut => r?,
        };

        let usage = response.usage.clone();
        let _ = ctx
            .sink
            .send(StreamEvent::Done {
                usage: usage.clone(),
                thinking_signature: None,
            })
            .await;

        Ok(FinalResponse {
            usage,
            thinking_signature: None,
            full_text: response.content,
            full_thinking: response.thinking,
            tool_calls: response.tool_calls.unwrap_or_default(),
        })
    }
}

fn build_model_config(request: &ChatRequest) -> ModelConfig {
    ModelConfig {
        model: request.model_id.clone(),
        temperature: request.temperature,
        max_tokens: request.max_tokens,
        reasoning: request.reasoning,
        system_prompt: Some(request.system_prompt.clone()),
        dynamic_system_suffix: request.instructions.clone(),
        ..Default::default()
    }
}

fn forward_callback(sink: tokio::sync::mpsc::Sender<StreamEvent>) -> StreamCallback {
    Arc::new(move |event: ModelStreamEvent| {
        let sink = sink.clone();
        let mapped = match event {
            ModelStreamEvent::Text(s) => StreamEvent::Text(s),
            ModelStreamEvent::Reasoning(chunk) => StreamEvent::Reasoning(ReasoningChunk {
                text: chunk.text,
                signature: chunk.signature,
            }),
            ModelStreamEvent::ToolCall(tc) => StreamEvent::ToolCall(tc),
            ModelStreamEvent::Done { tokens } => StreamEvent::Done {
                usage: if tokens > 0 {
                    Some(crate::models::TokenUsage {
                        prompt_tokens: 0,
                        completion_tokens: tokens,
                        total_tokens: tokens,
                    })
                } else {
                    None
                },
                thinking_signature: None,
            },
        };
        tokio::spawn(async move {
            let _ = sink.send(mapped).await;
        });
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_model_config_maps_fields() {
        let req = ChatRequest {
            model_id: "groq/llama-3.3-70b-versatile".to_string(),
            messages: vec![],
            system_prompt: "sys".to_string(),
            instructions: None,
            reasoning: crate::models::ReasoningLevel::Medium,
            temperature: 0.7,
            max_tokens: 4096,
            tools: vec![],
        };
        let cfg = build_model_config(&req);
        assert_eq!(cfg.model, "groq/llama-3.3-70b-versatile");
    }
}
