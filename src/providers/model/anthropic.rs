//! Anthropic provider — C4 wrapper over the v0.6 `AnthropicAdapter`.
//!
//! Same pattern as `ollama.rs`: the adapter's internals (wire format,
//! cache_control blocks, extended-thinking signature round-trip) are
//! preserved verbatim; the wrapper just plumbs `ChatRequest` /
//! `StreamContext` into the legacy shapes.
//!
//! Anthropic is the one provider that emits a `thinking_signature`
//! that MUST round-trip on the next request. The adapter's
//! `ModelResponse.thinking_signature` already carries it; we forward
//! that onto the `FinalResponse` so the reducer can commit it via
//! `ChatMessage::with_thinking_signature`.

use std::sync::Arc;

use async_trait::async_trait;

use crate::domain::ChatRequest;
use crate::models::adapters::anthropic::AnthropicAdapter;
use crate::models::{
    Model, ModelConfig, ModelError, ReasoningChunk, Result, StreamCallback,
    StreamEvent as ModelStreamEvent,
};

use super::super::capabilities::Capabilities;
use super::super::ctx::{FinalResponse, StreamContext, StreamEvent};
use super::ModelProvider;

/// Anthropic adapter fronted by `ModelProvider`.
pub struct AnthropicProvider {
    adapter: AnthropicAdapter,
    capabilities: Capabilities,
}

impl AnthropicProvider {
    pub fn new(api_key: String, model_name: String, base_url: String) -> Result<Self> {
        let adapter = AnthropicAdapter::new(api_key, model_name, base_url)?;
        let capabilities =
            Capabilities::from_legacy(adapter.capabilities()).with_thinking_signature();
        Ok(Self {
            adapter,
            capabilities,
        })
    }
}

#[async_trait]
impl ModelProvider for AnthropicProvider {
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
        let thinking_signature = response.thinking_signature.clone();
        let _ = ctx
            .sink
            .send(StreamEvent::Done {
                usage: usage.clone(),
                thinking_signature: thinking_signature.clone(),
            })
            .await;

        Ok(FinalResponse {
            usage,
            thinking_signature,
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
            model_id: "anthropic/claude-opus-4-7".to_string(),
            messages: vec![],
            system_prompt: "sys".to_string(),
            instructions: Some("MERMAID.md content".to_string()),
            reasoning: crate::models::ReasoningLevel::XHigh,
            temperature: 0.7,
            max_tokens: 8192,
            tools: vec![],
        };
        let cfg = build_model_config(&req);
        assert_eq!(cfg.reasoning, crate::models::ReasoningLevel::XHigh);
        assert_eq!(cfg.max_tokens, 8192);
        assert_eq!(
            cfg.dynamic_system_suffix.as_deref(),
            Some("MERMAID.md content")
        );
    }
}
