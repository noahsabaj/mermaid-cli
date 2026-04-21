//! Gemini provider — C4 wrapper over the v0.6 `GeminiAdapter`.
//!
//! Google's Gemini family uses a different wire format from OpenAI-
//! compat (`:streamGenerateContent?alt=sse` + protobuf-ish JSON
//! shape). The v0.6 adapter handles all of that; we wrap and
//! forward.

use std::sync::Arc;

use async_trait::async_trait;

use crate::domain::ChatRequest;
use crate::models::adapters::gemini::GeminiAdapter;
use crate::models::{
    Model, ModelConfig, ModelError, ReasoningChunk, Result, StreamCallback,
    StreamEvent as ModelStreamEvent,
};

use super::super::capabilities::Capabilities;
use super::super::ctx::{FinalResponse, StreamContext, StreamEvent};
use super::ModelProvider;

pub struct GeminiProvider {
    adapter: GeminiAdapter,
    capabilities: Capabilities,
}

impl GeminiProvider {
    pub fn new(api_key: String, model_name: String, base_url: String) -> Result<Self> {
        let adapter = GeminiAdapter::new(api_key, model_name, base_url)?;
        let capabilities = Capabilities::from_legacy(adapter.capabilities());
        Ok(Self {
            adapter,
            capabilities,
        })
    }
}

#[async_trait]
impl ModelProvider for GeminiProvider {
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
        tools: request.tools.iter().map(|t| t.to_openai_json()).collect(),
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
            model_id: "gemini/gemini-3.1-pro-preview".to_string(),
            messages: vec![],
            system_prompt: "sys".to_string(),
            instructions: None,
            reasoning: crate::models::ReasoningLevel::High,
            temperature: 0.5,
            max_tokens: 4096,
            tools: vec![],
        };
        let cfg = build_model_config(&req);
        assert_eq!(cfg.reasoning, crate::models::ReasoningLevel::High);
        assert_eq!(cfg.temperature, 0.5);
        assert!(cfg.dynamic_system_suffix.is_none());
    }
}
