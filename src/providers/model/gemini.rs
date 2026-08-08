//! Gemini provider — wraps `models::adapters::gemini::GeminiAdapter`.
//!
//! Google's Gemini family uses a different wire format from OpenAI-
//! compat (`:streamGenerateContent?alt=sse` + protobuf-ish JSON
//! shape). The adapter handles all of that; this wrapper just
//! forwards.

use async_trait::async_trait;

use mermaid_domain::ChatRequest;
use mermaid_model::models::adapters::gemini::GeminiAdapter;
use mermaid_model::models::{Model, ModelConfig, ModelError, Result};

use super::super::ctx::{FinalResponse, StreamContext, StreamEvent};
use super::{ContextSizing, ModelProvider, resolve_limits_cached};
use mermaid_model::models::ModelCapabilities;

/// Gemini's AI Studio root, and the env vars its key lives in. `LEGACY_API_KEY_ENV`
/// predates Google's rename and is still accepted when `GOOGLE_API_KEY` is unset —
/// but only when the user has not pointed at a specific var themselves.
pub const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";
pub const DEFAULT_API_KEY_ENV: &str = "GOOGLE_API_KEY";
pub const LEGACY_API_KEY_ENV: &str = "GEMINI_API_KEY";

pub struct GeminiProvider {
    adapter: GeminiAdapter,
    capabilities: ModelCapabilities,
}

impl GeminiProvider {
    /// Wrap a fresh [`GeminiAdapter`] as a `ModelProvider`.
    ///
    /// # Errors
    ///
    /// Only [`GeminiAdapter::new`]'s — the HTTP client build. The API is not
    /// contacted here, so an invalid key or unreachable `base_url` still
    /// constructs and fails on the first request.
    pub fn new(api_key: String, model_name: String, base_url: String) -> Result<Self> {
        let adapter = GeminiAdapter::new(api_key, model_name, base_url)?;
        let capabilities = adapter.capabilities().clone();
        Ok(Self {
            adapter,
            capabilities,
        })
    }
}

#[async_trait]
impl ModelProvider for GeminiProvider {
    fn capabilities(&self) -> &ModelCapabilities {
        &self.capabilities
    }

    /// Live limit discovery via Gemini's models endpoint (`GET {base}/models/
    /// {id}` → `inputTokenLimit` window + `outputTokenLimit` output ceiling).
    /// Cache-first via `provider_probes` (TTL-bounded), one live fetch on a
    /// miss; a fetch failure resolves all-`None`.
    async fn resolve_context_window(&self, request: &ChatRequest) -> ContextSizing {
        let _ = request;
        let model = Model::name(&self.adapter).to_string();
        let limits =
            resolve_limits_cached("gemini", &model, || self.adapter.fetch_model_limits()).await;
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
        let stop_reason = response.stop_reason.clone();
        // Terminal Done through the ordered relay, then drain (see openai_compat).
        let _ = relay_tx.send(StreamEvent::Done {
            usage: usage.clone(),
            provider_continuation: None,
            stop_reason: stop_reason.clone(),
        });
        drop(relay_tx);
        mermaid_model::utils::join_logged(relay_handle.take(), "stream_relay").await;

        Ok(FinalResponse {
            usage,
            provider_continuation: None,
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
            model_id: "gemini/gemini-3.1-pro-preview".to_string(),
            messages: vec![],
            system_prompt: "sys".to_string(),
            instructions: None,
            reasoning: mermaid_model::models::ReasoningLevel::High,
            temperature: 0.5,
            max_tokens: 4096,
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
        assert_eq!(cfg.reasoning, mermaid_model::models::ReasoningLevel::High);
        assert_eq!(cfg.temperature, 0.5);
        assert!(cfg.dynamic_system_suffix.is_none());
    }
}
