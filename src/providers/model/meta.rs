//! Meta provider — wraps `models::adapters::meta::MetaAdapter`.
//!
//! Same pattern as its four siblings now: the adapter owns the wire format
//! (the Responses endpoint, stateless encrypted-reasoning replay); this
//! wrapper plumbs `ChatRequest` / `StreamContext` into it.
//!
//! Meta is the second provider that emits a `provider_continuation` which
//! MUST round-trip — the encrypted reasoning items, replayed verbatim so
//! thinking survives a tool turn. The adapter's
//! `ModelResponse.provider_continuation` carries it; we forward that onto
//! the `FinalResponse` so the reducer can commit it.

use std::collections::HashMap;

use async_trait::async_trait;

use mermaid_domain::{ChatRequest, ToolDefinition};
use mermaid_model::models::adapters::meta::MetaAdapter;
use mermaid_model::models::{Model, ModelCapabilities, ModelConfig, ModelError, Result};

use super::super::ctx::{FinalResponse, StreamContext, StreamEvent};
use super::ModelProvider;

pub use mermaid_model::models::adapters::meta::{DEFAULT_API_KEY_ENV, DEFAULT_BASE_URL};

/// Meta adapter fronted by `ModelProvider`.
pub struct MetaProvider {
    adapter: MetaAdapter,
    capabilities: ModelCapabilities,
}

impl MetaProvider {
    /// Wrap a fresh [`MetaAdapter`] as a `ModelProvider`.
    ///
    /// # Errors
    ///
    /// Only [`MetaAdapter::new`]'s — the HTTP client build. The API is not
    /// contacted here, so an invalid key or unreachable `base_url` still
    /// constructs and fails on the first request.
    pub fn new(
        api_key: String,
        model_name: String,
        base_url: String,
        extra_headers: HashMap<String, String>,
    ) -> Result<Self> {
        let adapter = MetaAdapter::new(api_key, model_name, base_url, extra_headers)?;
        let capabilities = Model::capabilities(&adapter).clone();
        Ok(Self {
            adapter,
            capabilities,
        })
    }
}

#[async_trait]
impl ModelProvider for MetaProvider {
    fn capabilities(&self) -> &ModelCapabilities {
        &self.capabilities
    }

    async fn chat(&self, request: ChatRequest, ctx: StreamContext) -> Result<FinalResponse> {
        let config = build_model_config(&request);
        let chat_fut = self
            .adapter
            .chat(&request.messages, &config, Some(ctx.sink.clone()));

        // Meta used to select on the token inside its own read loop. The
        // outer race is equivalent and is what the other four do: dropping
        // this future drops the response stream with it.
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
        // F3: the terminal Done goes on the same sink the adapter just
        // finished writing to, so it cannot overtake a still-queued ToolCall.
        let _ = ctx
            .sink
            .send(StreamEvent::Done {
                usage: usage.clone(),
                provider_continuation: provider_continuation.clone(),
                stop_reason: stop_reason.clone(),
            })
            .await;

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
        tools: request
            .tools
            .iter()
            .map(ToolDefinition::to_openai_json)
            .collect(),
        resolved_context_window: request.resolved_context_window,
        resolved_max_output: request.resolved_max_output,
        output_schema: request.output_schema.clone(),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mermaid_domain::ToolDefinition;

    #[test]
    fn build_model_config_maps_fields() {
        let req = ChatRequest {
            model_id: "meta/muse-spark-1.1".to_string(),
            messages: vec![],
            system_prompt: "system".to_string(),
            instructions: Some("project".to_string()),
            reasoning: mermaid_model::models::ReasoningLevel::Max,
            temperature: 0.7,
            max_tokens: 200_000,
            tools: vec![ToolDefinition {
                name: "read_file".to_string(),
                description: "Read a file".to_string(),
                input_schema: serde_json::json!({"type": "object"}),
            }],
            ollama_num_ctx: None,
            ollama_allow_ram_offload: None,
            resolved_context_window: Some(mermaid_model::constants::META_MUSE_SPARK_CONTEXT_WINDOW),
            resolved_max_output: Some(mermaid_model::constants::META_MUSE_SPARK_MAX_OUTPUT_TOKENS),
            output_schema: None,
            suppress_auto_compact: false,
            suppressed_builtin_tools: Vec::new(),
        };
        let cfg = build_model_config(&req);
        assert_eq!(cfg.dynamic_system_suffix.as_deref(), Some("project"));
        assert_eq!(
            cfg.resolved_max_output,
            Some(mermaid_model::constants::META_MUSE_SPARK_MAX_OUTPUT_TOKENS)
        );
        // The adapter unwraps this envelope into the Responses shape; the
        // wrapper's job is only to produce it the same way every other
        // provider does.
        assert_eq!(cfg.tools[0]["function"]["name"], "read_file");
    }
}
