//! Anthropic provider — wraps `models::adapters::anthropic::AnthropicAdapter`.
//!
//! Same pattern as `ollama.rs`: the adapter handles the wire format
//! (cache_control blocks, extended-thinking signature round-trip);
//! this wrapper plumbs `ChatRequest` / `StreamContext` into it.
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
use super::{ContextSizing, ModelProvider, resolve_limits_cached};

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

    /// Live limit discovery via Anthropic's Models API (`GET /v1/models/
    /// {id}` → `max_input_tokens` window + `max_tokens` output ceiling).
    /// Cache-first via `provider_probes` (TTL-bounded), one live fetch on a
    /// miss; a fetch failure resolves all-`None` (adapter floors apply).
    async fn resolve_context_window(&self, request: &ChatRequest) -> ContextSizing {
        let _ = request;
        let model = Model::name(&self.adapter).to_string();
        let limits =
            resolve_limits_cached("anthropic", &model, || self.adapter.fetch_model_limits()).await;
        let window = limits.as_ref().and_then(|l| l.max_context_tokens);
        ContextSizing {
            model_max: window,
            effective: window,
            source: None,
            max_output: limits.as_ref().and_then(|l| l.max_output_tokens),
        }
    }

    async fn chat(&self, request: ChatRequest, ctx: StreamContext) -> Result<FinalResponse> {
        let config = build_model_config(&request);
        // F2: ordered relay — see stream_bridge docs.
        let (relay_tx, relay_handle) = super::stream_bridge::ordered_relay(ctx.sink.clone());
        let callback = forward_callback(relay_tx.clone());
        let chat_fut = self
            .adapter
            .chat(&request.messages, &config, Some(callback));

        let response = tokio::select! {
            biased;
            _ = ctx.token.cancelled() => {
                return Err(ModelError::Cancelled);
            },
            r = chat_fut => r?,
        };

        let usage = response.usage.clone();
        let thinking_signature = response.thinking_signature.clone();
        let stop_reason = response.stop_reason.clone();
        // Terminal Done through the ordered relay, then drain (see stream_bridge).
        let _ = relay_tx.send(StreamEvent::Done {
            usage: usage.clone(),
            thinking_signature: thinking_signature.clone(),
            stop_reason: stop_reason.clone(),
        });
        drop(relay_tx);
        crate::utils::join_logged(relay_handle.take(), "stream_relay").await;

        Ok(FinalResponse {
            usage,
            thinking_signature,
            tool_calls: response.tool_calls.unwrap_or_default(),
            stop_reason,
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
        resolved_context_window: request.resolved_context_window,
        resolved_max_output: request.resolved_max_output,
        ..Default::default()
    }
}

fn forward_callback(sink: tokio::sync::mpsc::UnboundedSender<StreamEvent>) -> StreamCallback {
    Arc::new(move |event: ModelStreamEvent| {
        let mapped = match event {
            ModelStreamEvent::Text(s) => StreamEvent::Text(s),
            ModelStreamEvent::Reasoning(chunk) => StreamEvent::Reasoning(ReasoningChunk {
                text: chunk.text,
                signature: chunk.signature,
            }),
            ModelStreamEvent::ToolCall(tc) => StreamEvent::ToolCall(tc),
            ModelStreamEvent::Status(s) => StreamEvent::Status(s),
            ModelStreamEvent::Done { tokens } => StreamEvent::Done {
                usage: if tokens > 0 {
                    Some(crate::models::TokenUsage::provider(0, tokens, tokens))
                } else {
                    None
                },
                thinking_signature: None,
                stop_reason: None,
            },
        };
        let _ = sink.send(mapped);
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

            ollama_num_ctx: None,
            ollama_allow_ram_offload: None,
            resolved_context_window: None,
            resolved_max_output: None,
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
