//! Ollama provider — C3 wrapper over the existing `OllamaAdapter`.
//!
//! This is a delegating shim. It satisfies the v0.7 `ModelProvider`
//! trait by translating `ChatRequest` + `StreamContext` into the
//! shapes the v0.6 adapter expects, and forwarding streaming events
//! back through the typed sink. In C4 the adapter's internals get
//! folded directly under this file; in C10 the old
//! `src/models/adapters/ollama.rs` is deleted entirely. Until then,
//! the two live side by side.
//!
//! Why a wrapper instead of a rewrite here? The wire format,
//! gpt-oss dispatch, NDJSON framing, truncation marker, and retry
//! logic in the v0.6 adapter are all correct and tested. Re-typing
//! 700 lines of that up-front bakes in a diff the size of the
//! architecture change itself. The wrapper pattern lets us prove
//! the trait shape end-to-end now, and mechanical move later.

use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

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
}

impl OllamaProvider {
    /// Construct from a model name + backend config, delegating to
    /// the existing adapter's async constructor (which probes the
    /// Ollama URL and builds a reusable HTTP client).
    pub async fn new(model_name: &str, backend: Arc<BackendConfig>) -> Result<Self> {
        let adapter = OllamaAdapter::new(model_name, backend).await?;
        let capabilities = Capabilities::from_legacy(adapter.capabilities());
        Ok(Self {
            adapter,
            capabilities,
        })
    }
}

#[async_trait]
impl ModelProvider for OllamaProvider {
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    async fn chat(&self, request: ChatRequest, ctx: StreamContext) -> Result<FinalResponse> {
        let config = build_model_config(&request);
        let callback = stream_callback_for(ctx.sink.clone());

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
                // Synthesize a Done event so the reducer cleanly
                // transitions out of Generating rather than hanging.
                let _ = ctx.sink.send(StreamEvent::Done {
                    usage: None,
                    thinking_signature: None,
                }).await;
                return Err(ModelError::StreamError(
                    "cancelled by user".to_string(),
                ));
            },
            r = chat_fut => r?,
        };

        // Emit a final typed Done event. The adapter's streaming
        // callback emits `ModelStreamEvent::Done` for us, but if it
        // fell through a non-streaming path we still owe one here.
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

// ─── helpers ────────────────────────────────────────────────────────

fn build_model_config(request: &ChatRequest) -> ModelConfig {
    ModelConfig {
        model: request.model_id.clone(),
        temperature: request.temperature,
        max_tokens: request.max_tokens,
        reasoning: request.reasoning,
        system_prompt: Some(request.system_prompt.clone()),
        dynamic_system_suffix: request.instructions.clone(),
        ..Default::default()
    }
}

/// Build a `StreamCallback` that forwards `ModelStreamEvent`s from
/// the adapter into the typed `StreamEvent` sink.
fn stream_callback_for(sink: tokio::sync::mpsc::Sender<StreamEvent>) -> StreamCallback {
    Arc::new(move |event: ModelStreamEvent| {
        let sink = sink.clone();
        // Adapter callbacks are sync but short — fire-and-forget the
        // send on a task. Using `blocking_send` on the mpsc would
        // work too but risks the runtime not having a runtime
        // handle in sync context.
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
        // `try_send` is non-blocking; if the channel is full, the
        // adapter's TCP read loop will naturally backpressure on the
        // *next* chunk (since we ignored this one). For now we do
        // `spawn + send` so backpressure is cleaner — a bounded
        // queue dropping events is worse than waiting for space.
        tokio::spawn(async move {
            let _ = sink.send(mapped).await;
        });
    })
}

/// Unused-warning suppressor — ensures the `_` binding doesn't trip
/// clippy by retaining a reference to the cancellation token even
/// when the select! branch isn't active.
#[allow(dead_code)]
fn _touch_token(token: &CancellationToken) {
    let _ = token.is_cancelled();
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
        let cfg = build_model_config(&req);
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

    #[tokio::test]
    async fn stream_callback_forwards_text_event() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
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
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
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
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
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
