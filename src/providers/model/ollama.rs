//! Ollama provider wrapping `OllamaAdapter`.
//!
//! The adapter owns the wire format (NDJSON framing, gpt-oss
//! reasoning dispatch, truncation marker, retry). The wrapper
//! translates `ChatRequest` ↔ `ModelConfig` and bridges the
//! adapter's legacy `StreamCallback` to the typed `StreamEvent`
//! sink. Adapter-internals stay where they are; the architecture
//! boundary is at `ModelProvider::chat`.

use std::sync::Arc;

use async_trait::async_trait;

use crate::domain::ChatRequest;
use crate::models::adapters::ollama::OllamaAdapter;
use crate::models::{
    BackendConfig, Model, ModelConfig, ModelError, ReasoningChunk, Result, StreamCallback,
    StreamEvent as ModelStreamEvent,
};

use super::super::capabilities::Capabilities;
use super::super::ctx::{FinalResponse, StreamContext, StreamEvent};
use super::ModelProvider;

/// Ollama adapter fronted by `ModelProvider`.
pub struct OllamaProvider {
    adapter: OllamaAdapter,
    capabilities: Capabilities,
    /// Shared app `Config` so `build_model_config` can read Ollama
    /// hardware options (`num_ctx`, `num_gpu`, `num_thread`, `numa`) at
    /// call time. Before F11 these were silently dropped because the
    /// wrapper built `ModelConfig` only from `ChatRequest` fields.
    config: Arc<crate::app::Config>,
}

impl OllamaProvider {
    /// Backward-compatible constructor that uses a default app config.
    /// Call `with_app_config` instead when you have one available so
    /// Ollama hardware options actually reach the adapter.
    pub async fn new(model_name: &str, backend: Arc<BackendConfig>) -> Result<Self> {
        Self::with_app_config(model_name, backend, Arc::new(crate::app::Config::default())).await
    }

    /// Construct with an explicit `app::Config` reference. Used by
    /// `ProviderFactory::build_provider` so `config.ollama.{num_gpu,
    /// num_ctx, num_thread, numa}` make it into the Ollama request's
    /// `options` block.
    pub async fn with_app_config(
        model_name: &str,
        backend: Arc<BackendConfig>,
        config: Arc<crate::app::Config>,
    ) -> Result<Self> {
        let adapter = OllamaAdapter::new(model_name, backend).await?;
        let capabilities = Capabilities::from_legacy(adapter.capabilities());
        Ok(Self {
            adapter,
            capabilities,
            config,
        })
    }
}

#[async_trait]
impl ModelProvider for OllamaProvider {
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    async fn chat(&self, request: ChatRequest, ctx: StreamContext) -> Result<FinalResponse> {
        let config = build_model_config(&request, &self.config);
        // Ordered relay (F2): the adapter's sync callback pushes into an
        // `UnboundedSender` (synchronous, FIFO). A single relay task drains
        // into the bounded sink in order, avoiding the per-event `tokio::
        // spawn` race that could deliver `Done` before prior tool calls.
        let relay_tx = super::stream_bridge::ordered_relay(ctx.sink.clone());
        let callback = stream_callback_for(relay_tx);

        // Race adapter.chat against the cancellation token. When
        // cancelled, the adapter's stream loop observes the sink
        // closing (we drop `callback`) and exits at its next await.
        // This is the crucial structural win vs. the old
        // `check_interrupt` polling: the adapter doesn't need to
        // know anything about turn IDs — the sink either drains or
        // doesn't, and the tokens handle everything else.
        let chat_fut = self
            .adapter
            .chat(&request.messages, &config, Some(callback));

        let response = tokio::select! {
            biased;
            _ = ctx.token.cancelled() => {
                // Terminal event for a cancelled turn comes from the
                // runner's `drop_scope` once the `TurnScope` drains, so
                // we neither emit `StreamEvent::Done` here nor surface
                // an `UpstreamError`. `ModelError::Cancelled` is the
                // sentinel the runner swallows.
                return Err(ModelError::Cancelled);
            },
            r = chat_fut => r?,
        };

        // F3: the wrapper's `Done` is now the sole terminal event —
        // the adapter no longer emits one from the callback. Carrying
        // `thinking_signature` out of `ModelResponse` here is what
        // lets multi-turn extended thinking round-trip.
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
            tool_calls: response.tool_calls.unwrap_or_default(),
        })
    }
}

// ─── helpers ────────────────────────────────────────────────────────

fn build_model_config(request: &ChatRequest, app_config: &crate::app::Config) -> ModelConfig {
    let mut mc = ModelConfig {
        model: request.model_id.clone(),
        temperature: request.temperature,
        max_tokens: request.max_tokens,
        reasoning: request.reasoning,
        system_prompt: Some(request.system_prompt.clone()),
        dynamic_system_suffix: request.instructions.clone(),
        tools: request.tools.iter().map(|t| t.to_openai_json()).collect(),
        ..Default::default()
    };
    // F11: forward Ollama hardware options from the user's app config.
    // Previously these fields were configured + persisted but silently
    // ignored because this wrapper built ModelConfig in isolation.
    if let Some(v) = app_config.ollama.num_gpu {
        mc.set_backend_option("ollama".into(), "num_gpu".into(), v.to_string());
    }
    if let Some(v) = app_config.ollama.num_ctx {
        mc.set_backend_option("ollama".into(), "num_ctx".into(), v.to_string());
    }
    if let Some(v) = app_config.ollama.num_thread {
        mc.set_backend_option("ollama".into(), "num_thread".into(), v.to_string());
    }
    if let Some(v) = app_config.ollama.numa {
        mc.set_backend_option("ollama".into(), "numa".into(), v.to_string());
    }
    mc
}

/// Build a `StreamCallback` that forwards `ModelStreamEvent`s from the
/// adapter into an `UnboundedSender<StreamEvent>` (ordered relay). The
/// caller wires that sender to a bounded sink via
/// `stream_bridge::ordered_relay`; this keeps event delivery FIFO even
/// though the adapter calls us from a sync context.
fn stream_callback_for(sink: tokio::sync::mpsc::UnboundedSender<StreamEvent>) -> StreamCallback {
    Arc::new(move |event: ModelStreamEvent| {
        let mapped = match event {
            ModelStreamEvent::Text(s) => StreamEvent::Text(s),
            ModelStreamEvent::Reasoning(chunk) => StreamEvent::Reasoning(ReasoningChunk {
                text: chunk.text,
                signature: chunk.signature,
            }),
            ModelStreamEvent::ToolCall(tc) => StreamEvent::ToolCall(tc),
            ModelStreamEvent::Done { tokens } => StreamEvent::Done {
                usage: if tokens > 0 {
                    Some(crate::models::TokenUsage::provider(0, tokens, tokens))
                } else {
                    None
                },
                thinking_signature: None,
            },
        };
        // Synchronous send preserves ordering. Ignore errors — the
        // receiver has closed means the turn is already gone.
        let _ = sink.send(mapped);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_model_config_maps_request_fields() {
        let req = ChatRequest {
            model_id: "ollama/test".to_string(),
            messages: vec![],
            system_prompt: "sys".to_string(),
            instructions: Some("instructions text".to_string()),
            reasoning: crate::models::ReasoningLevel::High,
            temperature: 0.3,
            max_tokens: 2048,
            tools: vec![],
        };
        let app_cfg = crate::app::Config::default();
        let cfg = build_model_config(&req, &app_cfg);
        assert_eq!(cfg.model, "ollama/test");
        assert_eq!(cfg.temperature, 0.3);
        assert_eq!(cfg.max_tokens, 2048);
        assert_eq!(cfg.reasoning, crate::models::ReasoningLevel::High);
        assert_eq!(cfg.system_prompt.as_deref(), Some("sys"));
        assert_eq!(
            cfg.dynamic_system_suffix.as_deref(),
            Some("instructions text")
        );
    }

    /// F11 regression guard: Ollama hardware options in the user's
    /// app config must land in the ModelConfig's backend_options so
    /// the adapter's `build_request_body` emits them under `options`.
    /// Before F11 these were configured + persisted but never reached
    /// the wire — `num_ctx = 8192` in config.toml was a silent no-op.
    #[test]
    fn build_model_config_forwards_ollama_hardware_options() {
        let req = ChatRequest {
            model_id: "ollama/test".to_string(),
            messages: vec![],
            system_prompt: "sys".to_string(),
            instructions: None,
            reasoning: crate::models::ReasoningLevel::Medium,
            temperature: 0.7,
            max_tokens: 4096,
            tools: vec![],
        };
        let mut app_cfg = crate::app::Config::default();
        app_cfg.ollama.num_ctx = Some(8192);
        app_cfg.ollama.num_gpu = Some(10);
        app_cfg.ollama.num_thread = Some(8);
        app_cfg.ollama.numa = Some(true);

        let cfg = build_model_config(&req, &app_cfg);
        let opts = cfg.ollama_options();
        assert_eq!(opts.num_ctx, Some(8192));
        assert_eq!(opts.num_gpu, Some(10));
        assert_eq!(opts.num_thread, Some(8));
        assert_eq!(opts.numa, Some(true));
    }

    #[tokio::test]
    async fn stream_callback_forwards_text_event() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let cb = stream_callback_for(tx);
        cb(ModelStreamEvent::Text("hello".to_string()));
        let recv = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
            .await
            .expect("recv")
            .expect("sender alive");
        match recv {
            StreamEvent::Text(s) => assert_eq!(s, "hello"),
            _ => panic!("wrong variant"),
        }
    }

    #[tokio::test]
    async fn stream_callback_forwards_done_with_tokens() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let cb = stream_callback_for(tx);
        cb(ModelStreamEvent::Done { tokens: 42 });
        let recv = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
            .await
            .expect("recv")
            .expect("sender");
        match recv {
            StreamEvent::Done { usage, .. } => {
                let u = usage.expect("tokens > 0 → Some");
                assert_eq!(u.total_tokens, 42);
            },
            _ => panic!("wrong variant"),
        }
    }

    #[tokio::test]
    async fn stream_callback_done_zero_tokens_is_none_usage() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let cb = stream_callback_for(tx);
        cb(ModelStreamEvent::Done { tokens: 0 });
        let recv = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
            .await
            .expect("recv")
            .expect("sender");
        match recv {
            StreamEvent::Done { usage, .. } => assert!(usage.is_none()),
            _ => panic!("wrong variant"),
        }
    }
}
