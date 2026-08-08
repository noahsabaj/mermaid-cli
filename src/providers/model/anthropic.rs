//! Anthropic provider — wraps `models::adapters::anthropic::AnthropicAdapter`.
//!
//! Same pattern as `ollama.rs`: the adapter handles the wire format
//! (cache_control blocks, extended-thinking signature round-trip);
//! this wrapper plumbs `ChatRequest` / `StreamContext` into it.
//!
//! Anthropic is the one provider that emits a `provider_continuation`
//! that MUST round-trip on the next request. The adapter's
//! `ModelResponse.provider_continuation` already carries it; we forward
//! that onto the `FinalResponse` so the reducer can commit it via
//! `ChatMessage::with_provider_continuation`.

use async_trait::async_trait;

use mermaid_domain::ChatRequest;
use mermaid_model::models::adapters::anthropic::AnthropicAdapter;
use mermaid_model::models::{Model, ModelConfig, ModelError, Result};

use super::super::ctx::{FinalResponse, StreamContext, StreamEvent};
use super::{ContextSizing, ModelProvider, resolve_limits_cached};
use mermaid_model::models::ModelCapabilities;

/// Anthropic's Messages-API root, and the env var its key lives in.
///
/// One definition, next to the provider, like `meta`'s. `ProviderFactory`
/// builds the endpoint from these and `providers::discovery` reports it from
/// the same constants — the literal used to be spelled out separately in both.
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com/v1";
pub const DEFAULT_API_KEY_ENV: &str = "ANTHROPIC_API_KEY";

/// Anthropic adapter fronted by `ModelProvider`.
pub struct AnthropicProvider {
    adapter: AnthropicAdapter,
    capabilities: ModelCapabilities,
}

impl AnthropicProvider {
    pub fn new(api_key: String, model_name: String, base_url: String) -> Result<Self> {
        let adapter = AnthropicAdapter::new(api_key, model_name, base_url)?;
        let capabilities = adapter.capabilities().clone().with_provider_continuation();
        Ok(Self {
            adapter,
            capabilities,
        })
    }
}

#[async_trait]
impl ModelProvider for AnthropicProvider {
    fn capabilities(&self) -> &ModelCapabilities {
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
        let callback = super::stream_bridge::forward_callback(relay_tx.clone());
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
        let provider_continuation = response.provider_continuation.clone();
        let stop_reason = response.stop_reason.clone();
        // Terminal Done through the ordered relay, then drain (see stream_bridge).
        let _ = relay_tx.send(StreamEvent::Done {
            usage: usage.clone(),
            provider_continuation: provider_continuation.clone(),
            stop_reason: stop_reason.clone(),
        });
        drop(relay_tx);
        mermaid_model::utils::join_logged(relay_handle.take(), "stream_relay").await;

        Ok(FinalResponse {
            usage,
            provider_continuation,
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
        // The adapter maps this to `output_config.format` (native
        // structured output); client-side validation stays the final gate.
        output_schema: request.output_schema.clone(),
        ..Default::default()
    }
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
            reasoning: mermaid_model::models::ReasoningLevel::XHigh,
            temperature: 0.7,
            max_tokens: 8192,
            tools: vec![],

            ollama_num_ctx: None,
            ollama_allow_ram_offload: None,
            resolved_context_window: None,
            resolved_max_output: None,
            output_schema: None,
            suppress_auto_compact: false,
            suppressed_builtin_tools: Vec::new(),
        };
        let cfg = build_model_config(&req);
        assert_eq!(cfg.reasoning, mermaid_model::models::ReasoningLevel::XHigh);
        assert_eq!(cfg.max_tokens, 8192);
        assert_eq!(
            cfg.dynamic_system_suffix.as_deref(),
            Some("MERMAID.md content")
        );
    }
}
