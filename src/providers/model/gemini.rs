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
        let config = ModelConfig::from(&request);
        let chat_fut = self
            .adapter
            .chat(&request.messages, &config, Some(ctx.sink.clone()));

        let response = tokio::select! {
            biased;
            _ = ctx.token.cancelled() => {
                return Err(ModelError::Cancelled);
            },
            r = chat_fut => r?,
        };

        let usage = response.usage.clone();
        let stop_reason = response.stop_reason.clone();
        // The terminal Done goes on the same sink the adapter just finished
        // writing to, so it cannot overtake a still-queued ToolCall.
        let _ = ctx
            .sink
            .send(StreamEvent::Done {
                usage: usage.clone(),
                provider_continuation: None,
                stop_reason: stop_reason.clone(),
            })
            .await;

        Ok(FinalResponse {
            usage,
            provider_continuation: None,
            tool_calls: response.tool_calls.unwrap_or_default(),
            stop_reason,
        })
    }
}
