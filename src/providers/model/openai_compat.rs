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
use std::sync::Arc;

use async_trait::async_trait;

use crate::domain::ChatRequest;
use crate::models::adapters::openai_compat::OpenAICompatAdapter;
use crate::models::{
    Model, ModelConfig, ModelError, ProviderProfile, ReasoningChunk, Result, StreamCallback,
    StreamEvent as ModelStreamEvent,
};
use crate::runtime::{NewProviderProbe, RuntimeStore};

use super::super::capabilities::Capabilities;
use super::super::ctx::{FinalResponse, StreamContext, StreamEvent};
use super::{ContextSizing, ModelProvider, probe_is_stale};

pub struct OpenAICompatProvider {
    adapter: OpenAICompatAdapter,
    capabilities: Capabilities,
}

impl OpenAICompatProvider {
    pub fn new(
        profile: &'static ProviderProfile,
        base_url: String,
        api_key: Option<String>,
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

/// Limits learned from one `/models` probe, cached per (provider, model) in
/// `provider_probes`. A successful listing WITHOUT limit metadata (or with the
/// model absent) is cached too — as `None`s — so providers that don't expose
/// limits aren't re-fetched every turn. Fetch *failures* are never cached.
#[derive(serde::Serialize, serde::Deserialize)]
struct CachedLimits {
    max_context_tokens: Option<usize>,
    max_output_tokens: Option<usize>,
}

const LIMITS_PROBE_KEY: &str = "limits_probe";

/// Load fresh cached limits, off the async runtime. Best-effort → `None`.
async fn load_limits_from_db(provider: String, model: String) -> Option<CachedLimits> {
    tokio::task::spawn_blocking(move || {
        let store = RuntimeStore::open_default().ok()?;
        let rec = store
            .provider_probes()
            .get(&provider, &model, LIMITS_PROBE_KEY)
            .ok()??;
        if probe_is_stale(&rec.probed_at) {
            return None;
        }
        serde_json::from_str::<CachedLimits>(&rec.capability_value).ok()
    })
    .await
    .ok()
    .flatten()
}

/// Persist probed limits for subsequent sessions. Best-effort.
async fn save_limits_to_db(provider: String, model: String, limits: &CachedLimits) {
    let value = match serde_json::to_string(limits) {
        Ok(v) => v,
        Err(_) => return,
    };
    let _ = tokio::task::spawn_blocking(move || -> Option<()> {
        let store = RuntimeStore::open_default().ok()?;
        store
            .provider_probes()
            .upsert(NewProviderProbe {
                provider,
                model_id: model,
                capability_key: LIMITS_PROBE_KEY.into(),
                capability_value: value,
                confidence: "probed".into(),
                error: None,
            })
            .ok()?;
        Some(())
    })
    .await;
}

#[async_trait]
impl ModelProvider for OpenAICompatProvider {
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    /// Live limit discovery: most OpenAI-compatible providers attach the
    /// model's context window / output ceiling to their `/models` metadata
    /// (OpenRouter, Cloudflare, …) — data mermaid previously discarded.
    /// Cache-first via `provider_probes` (TTL-bounded), one live fetch on a
    /// miss, static fallback (all `None`) when the provider exposes nothing or
    /// the fetch fails.
    async fn resolve_context_window(&self, request: &ChatRequest) -> ContextSizing {
        let _ = request;
        let provider = self.adapter.provider_name().to_string();
        let model = Model::name(&self.adapter).to_string();
        let limits = match load_limits_from_db(provider.clone(), model.clone()).await {
            Some(cached) => Some(cached),
            None => match self.adapter.list_models_detailed().await {
                Ok(listings) => {
                    let found = listings.into_iter().find(|m| m.id == model);
                    let limits = CachedLimits {
                        max_context_tokens: found.as_ref().and_then(|m| m.max_context_tokens),
                        max_output_tokens: found.as_ref().and_then(|m| m.max_output_tokens),
                    };
                    save_limits_to_db(provider, model, &limits).await;
                    Some(limits)
                },
                // Network/parse failure: don't cache; fall back to static.
                Err(_) => None,
            },
        };
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
        let stop_reason = response.stop_reason.clone();
        // Route the terminal Done through the SAME ordered relay (not directly
        // on the bounded sink) so it can't overtake a still-buffered ToolCall,
        // then await the relay drain before returning.
        let _ = relay_tx.send(StreamEvent::Done {
            usage: usage.clone(),
            thinking_signature: None,
            stop_reason: stop_reason.clone(),
        });
        drop(relay_tx);
        crate::utils::join_logged(relay_handle.take(), "stream_relay").await;

        Ok(FinalResponse {
            usage,
            thinking_signature: None,
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
            model_id: "groq/llama-3.3-70b-versatile".to_string(),
            messages: vec![],
            system_prompt: "sys".to_string(),
            instructions: None,
            reasoning: crate::models::ReasoningLevel::Medium,
            temperature: 0.7,
            max_tokens: 4096,
            tools: vec![],

            ollama_num_ctx: None,
            ollama_allow_ram_offload: None,
        };
        let cfg = build_model_config(&req);
        assert_eq!(cfg.model, "groq/llama-3.3-70b-versatile");
    }
}
