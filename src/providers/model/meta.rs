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

use mermaid_domain::ChatRequest;
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
        let config = ModelConfig::from(&request);
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
