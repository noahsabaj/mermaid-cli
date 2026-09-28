//! Anthropic's server-side compaction, driven end to end against a
//! [`MockProvider`].
//!
//! Three things must hold: a turn that asks for it sends the edit with its
//! beta header; the `compaction` block the API returns comes back verbatim
//! on the next request, ahead of everything else in that turn; and a model
//! that refuses it is retried without it and remembered, so the harness can
//! compact on its own from then on.

use serde_json::{Value, json};

use super::anthropic::AnthropicAdapter;
use super::mock_http::{MockProvider, Reply};
use crate::models::ModelError;
use crate::models::config::{ModelConfig, NativeCompaction};
use crate::models::stream::StreamEvent;
use crate::models::traits::Model;
use crate::models::types::{ChatMessage, ModelResponse, ProviderContinuation};

const SSE: &str = "text/event-stream";

fn fixture(name: &str) -> String {
    let path = format!(
        "{}/tests/fixtures/streams/anthropic/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(path).expect("stream fixture under tests/fixtures/streams/anthropic")
}

fn adapter(provider: &MockProvider) -> AnthropicAdapter {
    AnthropicAdapter::new(
        "key".into(),
        "claude-nova-7".into(),
        format!("{}/v1", provider.url),
    )
    .expect("adapter")
}

fn compacting() -> ModelConfig {
    ModelConfig {
        native_compaction: Some(NativeCompaction {
            trigger_tokens: 170_000,
        }),
        ..ModelConfig::default()
    }
}

async fn chat(
    model: &dyn Model,
    history: &[ChatMessage],
    config: &ModelConfig,
) -> (Result<ModelResponse, ModelError>, Vec<StreamEvent>) {
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    let drain = tokio::spawn(async move {
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        events
    });
    let result = model.chat(history, config, Some(tx)).await;
    (result, drain.await.expect("drain"))
}

#[tokio::test]
async fn a_turn_that_asks_sends_the_edit_and_its_beta() {
    let provider = MockProvider::start(|_| Reply::stream(SSE, &fixture("text.sse"))).await;
    let adapter = adapter(&provider);
    let (result, _) = chat(&adapter, &[ChatMessage::user("hi")], &compacting()).await;
    result.expect("chat");

    let sent = provider.received();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].header("anthropic-beta"), Some("compact-2026-01-12"));
    assert_eq!(
        sent[0].body["context_management"],
        json!({"edits": [{
            "type": "compact_20260112",
            "trigger": {"type": "input_tokens", "value": 170_000},
        }]})
    );
}

#[tokio::test]
async fn a_turn_that_does_not_ask_sends_neither() {
    let provider = MockProvider::start(|_| Reply::stream(SSE, &fixture("text.sse"))).await;
    let adapter = adapter(&provider);
    let (result, _) = chat(
        &adapter,
        &[ChatMessage::user("hi")],
        &ModelConfig::default(),
    )
    .await;
    result.expect("chat");

    let sent = provider.received();
    assert_eq!(sent[0].header("anthropic-beta"), None);
    assert!(sent[0].body.get("context_management").is_none());
}

#[tokio::test]
async fn the_compaction_block_is_kept_and_replayed_verbatim() {
    let provider = MockProvider::start(|_| Reply::stream(SSE, &fixture("compaction.sse"))).await;
    let adapter = adapter(&provider);
    let (result, events) = chat(&adapter, &[ChatMessage::user("hi")], &compacting()).await;
    let response = result.expect("chat");
    assert_eq!(response.content, "Continuing.");

    // Streamed in two deltas, kept whole.
    let block =
        json!({"type": "compaction", "content": "Summary: the user is rewriting the parser."});
    let continuation = response
        .provider_continuation
        .clone()
        .expect("continuation");
    assert_eq!(continuation.anthropic_compaction(), Some(&block));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, StreamEvent::Status(s) if s.contains("compacted"))),
        "the user hears that the provider compacted"
    );

    // Next turn: the block leads the assistant turn that carried it.
    let history = vec![
        ChatMessage::user("hi"),
        ChatMessage::assistant(&response.content).with_provider_continuation(continuation),
        ChatMessage::user("go on"),
    ];
    let (result, _) = chat(&adapter, &history, &compacting()).await;
    result.expect("second chat");
    let sent = provider.bodies_to("/messages");
    let assistant = &sent[1]["messages"][1];
    assert_eq!(assistant["role"], "assistant");
    assert_eq!(assistant["content"][0], block);
    assert_eq!(assistant["content"][1]["text"], "Continuing.");
}

#[tokio::test]
async fn a_model_that_refuses_it_is_retried_without_it_and_remembered() {
    let provider = MockProvider::start(|r| {
        if r.body.get("context_management").is_some() {
            return Reply::error(
                400,
                &json!({"type": "error", "error": {
                    "type": "invalid_request_error",
                    "message": "context_management: compaction is not supported for this model.",
                }}),
            );
        }
        Reply::stream(SSE, &fixture("text.sse"))
    })
    .await;
    let adapter = adapter(&provider);
    assert!(adapter.compacts_natively(), "optimistic until refused");

    let (result, _) = chat(&adapter, &[ChatMessage::user("hi")], &compacting()).await;
    assert_eq!(result.expect("retry succeeds").content, "Hello, world");
    let sent = provider.received();
    assert_eq!(sent.len(), 2);
    assert!(sent[1].body.get("context_management").is_none());
    assert_eq!(sent[1].header("anthropic-beta"), None);
    assert!(
        !adapter.compacts_natively(),
        "the harness compacts for this model from now on"
    );

    // Even a turn that asks no longer sends it.
    let (result, _) = chat(&adapter, &[ChatMessage::user("hi")], &compacting()).await;
    result.expect("third call");
    assert_eq!(provider.received().len(), 3);
}

#[test]
fn only_what_follows_the_last_compaction_counts_as_context() {
    let compacted = |text: &str| {
        ChatMessage::assistant(text).with_provider_continuation(ProviderContinuation::Anthropic {
            signature: String::new(),
            compaction: Some(json!({"type": "compaction", "content": text})),
        })
    };
    let history = vec![
        ChatMessage::user("one"),
        compacted("first"),
        ChatMessage::user("two"),
        compacted("second"),
        ChatMessage::user("three"),
    ];
    let live = ChatMessage::since_provider_compaction(&history);
    let contents: Vec<&str> = live.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(contents, ["second", "three"]);

    let untouched = vec![ChatMessage::user("a"), ChatMessage::assistant("b")];
    assert_eq!(ChatMessage::since_provider_compaction(&untouched).len(), 2);
}

#[test]
fn a_continuation_without_compaction_serializes_as_before() {
    // Existing session logs carry `{provider, signature}` only; they must
    // still load, and a plain signature must not grow a `compaction` key.
    let old: ProviderContinuation =
        serde_json::from_value(json!({"provider": "anthropic", "signature": "sig"}))
            .expect("old shape loads");
    assert_eq!(old, ProviderContinuation::anthropic("sig".to_string()));
    let written: Value = serde_json::to_value(&old).expect("serialize");
    assert_eq!(
        written,
        json!({"provider": "anthropic", "signature": "sig"})
    );
}
