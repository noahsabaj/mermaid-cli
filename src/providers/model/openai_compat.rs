//! OpenAI-compatible provider — wraps
//! `models::adapters::openai_compat::OpenAICompatAdapter`.
//!
//! This provider covers the OpenAI long-tail: OpenRouter, Groq,
//! Fireworks, Together, custom vLLM endpoints, plus the user-defined
//! entries in `[providers.*]`. The adapter looks up a
//! `ProviderProfile` (registry entry) and applies per-provider
//! reasoning shapes (flat `reasoning_effort` vs nested `reasoning:
//! {effort}`). This wrapper just forwards.

use std::collections::HashMap;

use async_trait::async_trait;

use mermaid_domain::ChatRequest;
use mermaid_model::models::adapters::ModelLimits;
use mermaid_model::models::adapters::openai_compat::OpenAICompatAdapter;
use mermaid_model::models::{Model, ModelConfig, ModelError, ProviderProfile, Result};

use super::super::ctx::{FinalResponse, StreamContext, StreamEvent};
use super::{
    ContextSizing, ModelProvider, learn_output_cap, output_cap_from_error, resolve_limits_cached,
    retry_cap,
};
use mermaid_model::models::ModelCapabilities;

pub struct OpenAICompatProvider {
    adapter: OpenAICompatAdapter,
    capabilities: ModelCapabilities,
}

impl OpenAICompatProvider {
    /// Wrap a fresh [`OpenAICompatAdapter`] as a `ModelProvider`.
    ///
    /// # Errors
    ///
    /// Only [`OpenAICompatAdapter::new`]'s — the HTTP client build. The
    /// endpoint is not contacted here, so a wrong `base_url` or missing key
    /// still constructs and fails on the first request.
    pub fn new(
        profile: &'static ProviderProfile,
        base_url: String,
        api_key: Option<String>,
        model_name: String,
        extra_headers: HashMap<String, String>,
    ) -> Result<Self> {
        let adapter =
            OpenAICompatAdapter::new(profile, base_url, api_key, model_name, extra_headers)?;
        let capabilities = adapter.capabilities().clone();
        Ok(Self {
            adapter,
            capabilities,
        })
    }
}

#[async_trait]
impl ModelProvider for OpenAICompatProvider {
    fn capabilities(&self) -> &ModelCapabilities {
        &self.capabilities
    }

    /// Live limit discovery: most OpenAI-compatible providers attach the
    /// model's context window / output ceiling to their `/models` metadata
    /// (OpenRouter et al); Cloudflare exposes it on its account-level
    /// `models/search` endpoint instead (`list_models_for_limits` routes
    /// there). Cache-first via `provider_probes` (TTL-bounded), one live
    /// fetch on a miss, static fallback (all `None`) when the provider
    /// exposes nothing or the fetch fails.
    async fn resolve_context_window(&self, request: &ChatRequest) -> ContextSizing {
        let _ = request;
        let provider = self.adapter.provider_name().to_string();
        let model = Model::name(&self.adapter).to_string();
        let limits = resolve_limits_cached(&provider, &model, || async {
            let listings = self.adapter.list_models_for_limits().await?;
            let found = listings.into_iter().find(|m| m.id == model);
            Ok(ModelLimits {
                max_context_tokens: found.as_ref().and_then(|m| m.max_context_tokens),
                max_output_tokens: found.as_ref().and_then(|m| m.max_output_tokens),
            })
        })
        .await;
        let window = limits.as_ref().and_then(|l| l.max_context_tokens);
        ContextSizing {
            model_max: window,
            effective: window,
            source: None,
            max_output: limits.as_ref().and_then(|l| l.max_output_tokens),
        }
    }

    async fn supports_vision(&self) -> Option<bool> {
        // Report the model-driven capability (derived from the model id) so the
        // no-vision-model warning fires for text-only models and stays quiet for
        // genuine vision models — instead of the default `None` (never warns).
        Some(self.capabilities.supports_vision)
    }

    async fn chat(&self, request: ChatRequest, ctx: StreamContext) -> Result<FinalResponse> {
        let config = build_model_config(&request);
        let (relay_tx, relay_handle) = super::stream_bridge::ordered_relay(ctx.sink.clone());
        let callback = super::stream_bridge::forward_callback(relay_tx.clone());
        let chat_fut = async {
            match self
                .adapter
                .chat(&request.messages, &config, Some(callback.clone()))
                .await
            {
                Ok(response) => Ok(response),
                Err(err) => {
                    // Learn-from-400 parity with the Ollama wrapper. AUTO
                    // omits max_tokens here, so this fires mainly for
                    // explicit user caps above the model's real ceiling:
                    // learn the cap (persisted for later sizing), clamp,
                    // retry ONCE. A 400 streamed no events, so the relay is
                    // untouched and reusable.
                    let Some(cap) = output_cap_from_error(&err) else {
                        return Err(err);
                    };
                    let Some(clamped) = retry_cap(config.max_tokens, cap) else {
                        return Err(err);
                    };
                    let provider = self.adapter.provider_name().to_string();
                    let model = Model::name(&self.adapter).to_string();
                    learn_output_cap(provider, model.clone(), cap).await;
                    let _ = relay_tx.send(StreamEvent::Status(format!(
                        "{model} rejected the output budget; learned its {cap}-token cap and retrying"
                    )));
                    let retry_config = ModelConfig {
                        max_tokens: clamped,
                        ..config.clone()
                    };
                    self.adapter
                        .chat(&request.messages, &retry_config, Some(callback.clone()))
                        .await
                },
            }
        };

        let response = tokio::select! {
            biased;
            _ = ctx.token.cancelled() => {
                return Err(ModelError::Cancelled);
            },
            r = chat_fut => r?,
        };

        let usage = response.usage.clone();
        let stop_reason = response.stop_reason.clone();
        // Route the terminal Done through the SAME ordered relay (not directly
        // on the bounded sink) so it can't overtake a still-buffered ToolCall,
        // then await the relay drain before returning.
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
            model_id: "groq/llama-3.3-70b-versatile".to_string(),
            messages: vec![],
            system_prompt: "sys".to_string(),
            instructions: None,
            reasoning: mermaid_model::models::ReasoningLevel::Medium,
            temperature: 0.7,
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
        assert_eq!(cfg.model, "groq/llama-3.3-70b-versatile");
    }
}
