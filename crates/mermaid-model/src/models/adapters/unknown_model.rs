//! A model nobody has written a catalog row for works on its first call.
//!
//! Every adapter is driven end to end (request built, sent over real HTTP to
//! a [`MockProvider`], response streamed back through the real driver) with
//! a model name the catalog has never heard of. Two things must hold:
//!
//! - A provider that accepts the request needs no retry: an unknown name
//!   costs nothing.
//! - A provider that rejects an optional parameter with a 400 gets a second
//!   request without it (or with it stepped down), the call succeeds, and
//!   the lesson is remembered so the next call doesn't pay again.
//!
//! The rejection bodies are the providers' own error shapes, so these also
//! pin that each adapter's blame reading understands its provider.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{Value, json};

use super::anthropic::AnthropicAdapter;
use super::gemini::GeminiAdapter;
use super::meta::MetaAdapter;
use super::mock_http::{MockProvider, Received, Reply};
use super::ollama::OllamaAdapter;
use super::openai_compat::OpenAICompatAdapter;
use crate::models::config::{BackendConfig, ModelConfig};
use crate::models::reasoning::ReasoningLevel;
use crate::models::stream::StreamEvent;
use crate::models::traits::Model;
use crate::models::types::{ChatMessage, ModelResponse};
use crate::models::{ModelError, lookup_provider};

const SSE: &str = "text/event-stream";
const NDJSON: &str = "application/x-ndjson";

fn fixture(provider: &str, name: &str) -> String {
    let path = format!(
        "{}/tests/fixtures/streams/{provider}/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// Run one streaming chat and collect what reached the sink.
async fn chat(
    model: &dyn Model,
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
    let result = model
        .chat(&[ChatMessage::user("hi")], config, Some(tx))
        .await;
    (result, drain.await.expect("drain"))
}

fn statuses(events: &[StreamEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::Status(s) => Some(s.clone()),
            _ => None,
        })
        .collect()
}

fn anthropic(provider: &MockProvider, model: &str) -> AnthropicAdapter {
    AnthropicAdapter::new("key".into(), model.into(), format!("{}/v1", provider.url))
        .expect("adapter")
}

fn openai(provider: &MockProvider, model: &str) -> OpenAICompatAdapter {
    OpenAICompatAdapter::new(
        lookup_provider("openai").expect("openai profile"),
        format!("{}/v1", provider.url),
        Some("key".into()),
        model.into(),
        HashMap::new(),
    )
    .expect("adapter")
}

fn gemini(provider: &MockProvider, model: &str) -> GeminiAdapter {
    GeminiAdapter::new(
        "key".into(),
        model.into(),
        format!("{}/v1beta", provider.url),
    )
    .expect("adapter")
}

fn meta(provider: &MockProvider, model: &str) -> MetaAdapter {
    MetaAdapter::new(
        "key".into(),
        model.into(),
        format!("{}/v1", provider.url),
        HashMap::new(),
    )
    .expect("adapter")
}

async fn ollama(provider: &MockProvider, model: &str) -> OllamaAdapter {
    let backend = BackendConfig {
        ollama_url: provider.url.clone(),
        timeout_secs: 5,
        max_idle_per_host: 1,
        ollama_autostart: false,
    };
    OllamaAdapter::new(model, Arc::new(backend))
        .await
        .expect("adapter")
}

/// Ollama's `/api/show`: a thinking-capable model.
fn ollama_show(request: &Received) -> Option<Reply> {
    request
        .path
        .ends_with("/api/show")
        .then(|| Reply::json(&json!({"capabilities": ["completion", "thinking"]})))
}

// --- An unknown name the provider accepts: one request, no retry ---

#[tokio::test]
async fn every_adapter_serves_an_unknown_model_on_the_first_request() {
    let config = ModelConfig::default();

    let provider =
        MockProvider::start(|_| Reply::stream(SSE, &fixture("anthropic", "text.sse"))).await;
    let (result, _) = chat(&anthropic(&provider, "claude-nova-7"), &config).await;
    assert_eq!(result.expect("anthropic").content, "Hello, world");
    assert_eq!(provider.bodies_to("/messages").len(), 1);

    let provider =
        MockProvider::start(|_| Reply::stream(SSE, &fixture("openai_compat", "text.sse"))).await;
    let (result, _) = chat(&openai(&provider, "nova-reasoner-1"), &config).await;
    assert_eq!(result.expect("openai").content, "Hello, world");
    assert_eq!(provider.bodies_to("/chat/completions").len(), 1);

    let provider =
        MockProvider::start(|_| Reply::stream(SSE, &fixture("gemini", "text.sse"))).await;
    let (result, _) = chat(&gemini(&provider, "gemini-9-ultra"), &config).await;
    assert_eq!(result.expect("gemini").content, "Hello, world");
    assert_eq!(provider.bodies_to(":streamGenerateContent").len(), 1);

    let provider = MockProvider::start(|_| Reply::stream(SSE, &fixture("meta", "text.sse"))).await;
    let (result, _) = chat(&meta(&provider, "muse-nova-2"), &config).await;
    assert_eq!(result.expect("meta").content, "Hello, world");
    assert_eq!(provider.bodies_to("/responses").len(), 1);

    let provider = MockProvider::start(|r| {
        ollama_show(r).unwrap_or_else(|| Reply::stream(NDJSON, &fixture("ollama", "text.ndjson")))
    })
    .await;
    let (result, _) = chat(&ollama(&provider, "brand-new:7b").await, &config).await;
    assert_eq!(result.expect("ollama").content, "Hello, world");
    assert_eq!(provider.bodies_to("/api/chat").len(), 1);
}

// --- An unknown name the provider rejects a parameter for: learn, retry ---

#[tokio::test]
async fn anthropic_learns_that_a_new_model_rejects_temperature() {
    let provider = MockProvider::start(|r| {
        if r.body.get("temperature").is_some() {
            return Reply::error(
                400,
                &json!({"type": "error", "error": {
                    "type": "invalid_request_error",
                    "message": "`temperature` is deprecated for this model.",
                }}),
            );
        }
        Reply::stream(SSE, &fixture("anthropic", "text.sse"))
    })
    .await;
    let adapter = anthropic(&provider, "claude-nova-7");
    let (result, events) = chat(&adapter, &ModelConfig::default()).await;
    assert_eq!(result.expect("retry succeeds").content, "Hello, world");

    let sent = provider.bodies_to("/messages");
    assert_eq!(sent.len(), 2);
    assert!(
        sent[0].get("temperature").is_some(),
        "optimistic first send"
    );
    assert!(sent[1].get("temperature").is_none());
    // Optimistic on the rest too: newest thinking shape, the asked-for effort.
    assert_eq!(sent[1]["thinking"]["type"], "adaptive");
    assert_eq!(sent[1]["output_config"]["effort"], "medium");
    assert!(adapter.param_memory().snapshot().contains("temperature"));
    let notices = statuses(&events);
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(notices[0].contains("claude-nova-7") && notices[0].contains("temperature"));

    // The next call remembers: one request, no temperature.
    let (result, _) = chat(&adapter, &ModelConfig::default()).await;
    result.expect("second call");
    let sent = provider.bodies_to("/messages");
    assert_eq!(sent.len(), 3);
    assert!(sent[2].get("temperature").is_none());
}

#[tokio::test]
async fn anthropic_steps_thinking_down_when_adaptive_is_rejected() {
    let provider = MockProvider::start(|r| {
        if r.body["thinking"]["type"] == "adaptive" {
            return Reply::error(
                400,
                &json!({"type": "error", "error": {
                    "type": "invalid_request_error",
                    "message": "thinking.type: Input tag 'adaptive' found using 'type' does not match any of the expected tags: 'disabled', 'enabled'",
                }}),
            );
        }
        Reply::stream(SSE, &fixture("anthropic", "text.sse"))
    })
    .await;
    let adapter = anthropic(&provider, "claude-nova-7");
    let (result, _) = chat(&adapter, &ModelConfig::default()).await;
    result.expect("retry succeeds");
    let sent = provider.bodies_to("/messages");
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1]["thinking"]["type"], "enabled");
    assert!(
        adapter
            .param_memory()
            .snapshot()
            .contains("thinking:adaptive")
    );
}

#[tokio::test]
async fn anthropic_signature_errors_are_not_read_as_unsupported_thinking() {
    let provider = MockProvider::start(|_| {
        Reply::error(
            400,
            &json!({"type": "error", "error": {
                "type": "invalid_request_error",
                "message": "messages.1.content.0.thinking.signature: Field required",
            }}),
        )
    })
    .await;
    let adapter = anthropic(&provider, "claude-nova-7");
    let (result, _) = chat(&adapter, &ModelConfig::default()).await;
    assert!(result.is_err());
    assert_eq!(provider.bodies_to("/messages").len(), 1, "no retry");
    assert!(adapter.param_memory().snapshot().is_empty());
}

#[tokio::test]
async fn openai_compat_learns_two_rejections_in_one_call() {
    // The o-series pattern: sampling params and the legacy budget spelling
    // both refused, one at a time, in OpenAI's own error envelope.
    let provider = MockProvider::start(|r| {
        if r.body.get("temperature").is_some() {
            return Reply::error(
                400,
                &json!({"error": {
                    "message": "Unsupported value: 'temperature' does not support 0.7 with this model. Only the default (1) value is supported.",
                    "type": "invalid_request_error",
                    "param": "temperature",
                    "code": "unsupported_value",
                }}),
            );
        }
        if r.body.get("max_tokens").is_some() {
            return Reply::error(
                400,
                &json!({"error": {
                    "message": "Unsupported parameter: 'max_tokens' is not supported with this model. Use 'max_completion_tokens' instead.",
                    "type": "invalid_request_error",
                    "param": "max_tokens",
                    "code": "unsupported_parameter",
                }}),
            );
        }
        Reply::stream(SSE, &fixture("openai_compat", "text.sse"))
    })
    .await;
    let adapter = openai(&provider, "nova-reasoner-1");
    let config = ModelConfig {
        max_tokens: 1_000,
        ..ModelConfig::default()
    };
    let (result, events) = chat(&adapter, &config).await;
    assert_eq!(result.expect("retry succeeds").content, "Hello, world");

    let sent = provider.bodies_to("/chat/completions");
    assert_eq!(sent.len(), 3);
    let last = &sent[2];
    assert!(last.get("temperature").is_none());
    assert!(last.get("max_tokens").is_none());
    assert_eq!(last["max_completion_tokens"], 1_000);
    assert_eq!(
        adapter.param_memory().snapshot(),
        crate::models::adapters::learning::Rejections::from(["max_tokens", "temperature"])
    );
    assert_eq!(statuses(&events).len(), 2);
}

#[tokio::test]
async fn openai_compat_steps_an_unsupported_effort_tier_down() {
    let provider = MockProvider::start(|r| {
        if r.body["reasoning_effort"] == "xhigh" {
            return Reply::error(
                400,
                &json!({"error": {
                    "message": "Invalid value for 'reasoning_effort': expected one of 'low', 'medium', 'high'.",
                    "type": "invalid_request_error",
                }}),
            );
        }
        Reply::stream(SSE, &fixture("openai_compat", "text.sse"))
    })
    .await;
    let adapter = openai(&provider, "nova-reasoner-1");
    let config = ModelConfig {
        reasoning: ReasoningLevel::XHigh,
        ..ModelConfig::default()
    };
    let (result, _) = chat(&adapter, &config).await;
    result.expect("retry succeeds");
    let sent = provider.bodies_to("/chat/completions");
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1]["reasoning_effort"], "high");
    // Only the tier was taken back, not reasoning altogether.
    let memory = adapter.param_memory().snapshot();
    assert!(memory.contains("reasoning_effort:xhigh"));
    assert!(!memory.contains("reasoning_effort"));
}

#[tokio::test]
async fn gemini_falls_back_from_thinking_level_to_budget() {
    let provider = MockProvider::start(|r| {
        if r.body.pointer("/generationConfig/thinkingConfig/thinkingLevel").is_some() {
            return Reply::error(
                400,
                &json!({"error": {
                    "code": 400,
                    "message": "Invalid JSON payload received. Unknown name \"thinkingLevel\" at 'generation_config.thinking_config': Cannot find field.",
                    "status": "INVALID_ARGUMENT",
                }}),
            );
        }
        Reply::stream(SSE, &fixture("gemini", "text.sse"))
    })
    .await;
    let adapter = gemini(&provider, "gemini-9-ultra");
    let (result, _) = chat(&adapter, &ModelConfig::default()).await;
    result.expect("retry succeeds");
    let sent = provider.bodies_to(":streamGenerateContent");
    assert_eq!(sent.len(), 2);
    assert!(
        sent[1]
            .pointer("/generationConfig/thinkingConfig/thinkingBudget")
            .is_some()
    );
    assert!(adapter.param_memory().snapshot().contains("thinking:level"));
}

#[tokio::test]
async fn ollama_drops_think_when_the_server_refuses_it() {
    // The capability probe says "thinking" (or failed, which assumes it),
    // but the server disagrees. Its error quotes the model id; that must
    // not be what the blame reads.
    let provider = MockProvider::start(|r| {
        if let Some(show) = ollama_show(r) {
            return show;
        }
        if r.body.get("think").is_some() {
            return Reply::error(
                400,
                &json!({"error": "\"brand-new:7b\" does not support thinking"}),
            );
        }
        Reply::stream(NDJSON, &fixture("ollama", "text.ndjson"))
    })
    .await;
    let adapter = ollama(&provider, "brand-new:7b").await;
    let (result, _) = chat(&adapter, &ModelConfig::default()).await;
    result.expect("retry succeeds");
    let sent = provider.bodies_to("/api/chat");
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0]["think"], true);
    assert!(sent[1].get("think").is_none());
    assert!(adapter.param_memory().snapshot().contains("think:bool"));
}

#[tokio::test]
async fn meta_steps_max_effort_down_to_xhigh() {
    let provider = MockProvider::start(|r| {
        if r.body["reasoning"]["effort"] == "max" {
            return Reply::error(
                400,
                &json!({"error": {
                    "message": "reasoning.effort 'max' is not supported for this model",
                    "type": "invalid_request_error",
                }}),
            );
        }
        Reply::stream(SSE, &fixture("meta", "text.sse"))
    })
    .await;
    let adapter = meta(&provider, "muse-nova-2");
    let config = ModelConfig {
        reasoning: ReasoningLevel::Max,
        ..ModelConfig::default()
    };
    let (result, _) = chat(&adapter, &config).await;
    result.expect("retry succeeds");
    let sent = provider.bodies_to("/responses");
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1]["reasoning"]["effort"], "xhigh");
    assert!(adapter.param_memory().snapshot().contains("effort:max"));
}

// --- The guards ---

#[tokio::test]
async fn an_unrelated_rejection_teaches_nothing() {
    let provider = MockProvider::start(|_| {
        Reply::error(
            400,
            &json!({"error": {"message": "messages: at least one message is required"}}),
        )
    })
    .await;
    let adapter = openai(&provider, "nova-reasoner-1");
    let (result, _) = chat(&adapter, &ModelConfig::default()).await;
    let err = result.expect_err("surfaces");
    assert!(err.to_string().contains("at least one message"), "{err}");
    assert_eq!(provider.bodies_to("/chat/completions").len(), 1);
    assert!(adapter.param_memory().snapshot().is_empty());
}

#[tokio::test]
async fn a_wrongly_blamed_parameter_is_not_remembered() {
    // The error mentions temperature, but taking it back doesn't help: the
    // request is refused for some other reason. Nothing proved the lesson,
    // so nothing is kept.
    let provider = MockProvider::start(|_| {
        Reply::error(
            400,
            &json!({"error": {"message": "temperature and top_p are fine; your account is not allowed to use this model"}}),
        )
    })
    .await;
    let adapter = openai(&provider, "nova-reasoner-1");
    let (result, _) = chat(&adapter, &ModelConfig::default()).await;
    assert!(result.is_err());
    assert_eq!(provider.bodies_to("/chat/completions").len(), 2);
    assert!(adapter.param_memory().snapshot().is_empty());
}

#[tokio::test]
async fn a_silently_ignored_parameter_is_never_learned_as_supported() {
    // Many OpenAI-compatible and local servers accept a parameter they don't
    // implement and ignore it. Silence is not a signal: success records
    // nothing, and the parameter keeps being sent as asked.
    let provider =
        MockProvider::start(|_| Reply::stream(SSE, &fixture("openai_compat", "text.sse"))).await;
    let adapter = openai(&provider, "nova-reasoner-1");
    for _ in 0..2 {
        let (result, _) = chat(&adapter, &ModelConfig::default()).await;
        result.expect("accepted");
    }
    assert!(adapter.param_memory().snapshot().is_empty());
    let sent = provider.bodies_to("/chat/completions");
    assert!(sent.iter().all(|b| b.get("temperature").is_some()));

    // Where silence IS the failure mode, the catalog hint decides the shape
    // instead: gpt-oss would take `think: true` and ignore it.
    let provider = MockProvider::start(|r| {
        ollama_show(r).unwrap_or_else(|| Reply::stream(NDJSON, &fixture("ollama", "text.ndjson")))
    })
    .await;
    let (result, _) = chat(
        &ollama(&provider, "gpt-oss:20b").await,
        &ModelConfig::default(),
    )
    .await;
    result.expect("accepted");
    assert_eq!(
        provider.bodies_to("/api/chat")[0]["think"],
        Value::from("medium")
    );
}
