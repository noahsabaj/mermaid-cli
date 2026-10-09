//! OpenAI on the Responses API, driven end to end against a [`MockProvider`].
//!
//! What must hold: requests go to `/responses` with no server-side state;
//! the reasoning a turn produced goes back with the next request; a native
//! `apply_patch` call reaches the reducer as Mermaid's `apply_patch` and goes
//! back to the model in the form it wrote, answered in kind; and what a model
//! refuses is taken back and remembered.

use std::collections::HashMap;

use serde_json::{Value, json};

use super::mock_http::{MockProvider, Reply};
use super::openai_compat::OpenAICompatAdapter;
use crate::models::ModelError;
use crate::models::config::{ModelConfig, NativeCompaction, NativeTools};
use crate::models::providers::lookup_provider;
use crate::models::stream::StreamEvent;
use crate::models::traits::Model;
use crate::models::types::{ChatMessage, ModelResponse};

const SSE: &str = "text/event-stream";
const MODEL: &str = "gpt-5.5";

fn fixture(name: &str) -> String {
    let path = format!(
        "{}/tests/fixtures/streams/openai_responses/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn adapter(provider: &MockProvider) -> OpenAICompatAdapter {
    OpenAICompatAdapter::new(
        lookup_provider("openai").expect("openai profile"),
        format!("{}/v1", provider.url),
        Some("key".into()),
        MODEL.into(),
        HashMap::new(),
    )
    .expect("adapter")
}

fn tool(name: &str) -> Value {
    json!({"type": "function", "function": {
        "name": name,
        "description": format!("{name} tool"),
        "parameters": {"type": "object", "properties": {}},
    }})
}

fn config() -> ModelConfig {
    ModelConfig {
        tools: ["read_file", "apply_patch", "execute_command"]
            .into_iter()
            .map(tool)
            .collect(),
        native_tools: NativeTools {
            text_editor: true,
            shell: true,
            computer: false,
        },
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

fn tool_types(body: &Value) -> Vec<String> {
    body["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|t| {
            t["name"]
                .as_str()
                .unwrap_or_else(|| t["type"].as_str().expect("type"))
                .to_string()
        })
        .collect()
}

#[tokio::test]
async fn openai_goes_to_responses_and_other_compat_providers_do_not() {
    let provider = MockProvider::start(|_| Reply::stream(SSE, &fixture("text.sse"))).await;
    let adapter = adapter(&provider);
    assert!(adapter.capabilities().emits_provider_continuation);
    let (result, _) = chat(&adapter, &[ChatMessage::user("hi")], &config()).await;
    let response = result.expect("chat");
    assert_eq!(response.content, "Hello, world");
    let continuation = response.provider_continuation.expect("continuation");
    assert!(continuation.openai_output(MODEL).is_some());

    let sent = provider.bodies_to("/responses");
    assert_eq!(sent.len(), 1);
    assert!(provider.bodies_to("/chat/completions").is_empty());
    assert_eq!(sent[0]["store"], false);
    assert_eq!(
        tool_types(&sent[0]),
        ["read_file", "execute_command", "apply_patch"]
    );

    let groq = OpenAICompatAdapter::new(
        lookup_provider("groq").expect("groq profile"),
        "http://127.0.0.1:9/v1".into(),
        Some("key".into()),
        "llama".into(),
        HashMap::new(),
    )
    .expect("adapter");
    assert!(!groq.capabilities().emits_provider_continuation);
    assert!(!groq.compacts_natively());
}

#[tokio::test]
async fn a_native_patch_runs_as_mermaids_and_goes_back_as_the_model_wrote_it() {
    let provider = MockProvider::start(|_| Reply::stream(SSE, &fixture("native_patch.sse"))).await;
    let adapter = adapter(&provider);
    let user = ChatMessage::user("rename fib");
    let (result, events) = chat(&adapter, std::slice::from_ref(&user), &config()).await;
    let response = result.expect("chat");

    let calls: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ToolCall(call) => Some(call.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(calls.len(), 1, "streamed once, not again from the terminal");
    assert_eq!(calls[0].function.name, "apply_patch");
    assert_eq!(
        calls[0].function.arguments["patch"],
        "*** Begin Patch\n*** Update File: lib/fib.py\n@@\n-def fib(n):\n+def fibonacci(n):\n*** End Patch"
    );
    assert_eq!(response.thinking.as_deref(), Some("Rename the function."));

    // The next request: the reducer committed the turn with its continuation
    // and the tool's result.
    let mut turn = ChatMessage::assistant("");
    turn.tool_calls = response.tool_calls.clone();
    let turn = turn.with_provider_continuation(response.provider_continuation.expect("state"));
    let history = [
        user,
        turn,
        ChatMessage::tool("call_p", "apply_patch", "Updated lib/fib.py"),
    ];
    let (result, _) = chat(&adapter, &history, &config()).await;
    result.expect("second turn");

    let input = provider.bodies_to("/responses")[1]["input"].clone();
    assert_eq!(input[0]["role"], "user");
    assert_eq!(input[1]["type"], "reasoning");
    assert_eq!(
        input[1]["encrypted_content"], "gAAAAopaque",
        "reasoning kept"
    );
    assert_eq!(input[2]["type"], "apply_patch_call");
    assert_eq!(input[2]["operation"]["path"], "lib/fib.py");
    assert!(input[2].get("id").is_none(), "store is false: no item ids");
    assert_eq!(
        input[3],
        json!({
            "type": "apply_patch_call_output",
            "call_id": "call_p",
            "status": "completed",
            "output": "Updated lib/fib.py",
        })
    );
    assert_eq!(input.as_array().map(Vec::len), Some(4));
}

#[tokio::test]
async fn a_model_without_apply_patch_gets_mermaids_schema_from_then_on() {
    let provider = MockProvider::start(|r| {
        let native = r.body["tools"]
            .as_array()
            .is_some_and(|tools| tools.iter().any(|t| t["type"] == "apply_patch"));
        if native {
            return Reply::error(
                400,
                &json!({"error": {
                    "message": "Tool 'apply_patch' is not supported with this model.",
                    "type": "invalid_request_error",
                    "param": "tools",
                }}),
            );
        }
        Reply::stream(SSE, &fixture("text.sse"))
    })
    .await;
    let adapter = adapter(&provider);
    let (result, events) = chat(&adapter, &[ChatMessage::user("hi")], &config()).await;
    result.expect("retry succeeds");
    let sent = provider.bodies_to("/responses");
    assert_eq!(sent.len(), 2);
    assert_eq!(
        tool_types(&sent[1]),
        ["read_file", "apply_patch", "execute_command"]
    );
    assert!(adapter.param_memory().snapshot().contains("native_tools"));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, StreamEvent::Status(s) if s.contains("apply_patch")))
    );
}

#[tokio::test]
async fn a_model_without_reasoning_sheds_each_reasoning_parameter() {
    // gpt-4.1's pattern: each reasoning parameter refused in turn.
    let provider = MockProvider::start(|r| {
        let refuse = |param: &str| {
            Reply::error(
                400,
                &json!({"error": {
                    "message": format!("Unsupported parameter: '{param}' is not supported with this model."),
                    "type": "invalid_request_error",
                    "param": param,
                }}),
            )
        };
        if r.body.pointer("/reasoning/effort").is_some() {
            return refuse("reasoning.effort");
        }
        if r.body.pointer("/reasoning/summary").is_some() {
            return refuse("reasoning.summary");
        }
        if r.body.get("include").is_some() {
            return refuse("include");
        }
        Reply::stream(SSE, &fixture("text.sse"))
    })
    .await;
    let adapter = OpenAICompatAdapter::new(
        lookup_provider("openai").expect("openai profile"),
        format!("{}/v1", provider.url),
        Some("key".into()),
        "gpt-4.1".into(),
        HashMap::new(),
    )
    .expect("adapter");
    let (result, _) = chat(&adapter, &[ChatMessage::user("hi")], &config()).await;
    result.expect("retries succeed");
    let sent = provider.bodies_to("/responses");
    assert_eq!(sent.len(), 4);
    assert!(sent[3].get("reasoning").is_none());
    assert!(sent[3].get("include").is_none());
    let memory = adapter.param_memory().snapshot();
    for item in ["effort", "reasoning_summary", "encrypted_reasoning"] {
        assert!(memory.contains(item), "{item} remembered");
    }
}

#[tokio::test]
async fn server_compaction_is_offered_until_refused() {
    let provider = MockProvider::start(|r| {
        if r.body.get("context_management").is_some() {
            return Reply::error(
                400,
                &json!({"error": {
                    "message": "Unknown parameter: 'context_management'.",
                    "type": "invalid_request_error",
                    "param": "context_management",
                }}),
            );
        }
        Reply::stream(SSE, &fixture("text.sse"))
    })
    .await;
    let adapter = adapter(&provider);
    assert!(adapter.compacts_natively(), "optimistic until refused");
    let cfg = ModelConfig {
        native_compaction: Some(NativeCompaction {
            trigger_tokens: 200_000,
        }),
        ..config()
    };
    let (result, _) = chat(&adapter, &[ChatMessage::user("hi")], &cfg).await;
    result.expect("retry succeeds");
    let sent = provider.bodies_to("/responses");
    assert_eq!(
        sent[0]["context_management"],
        json!([{"type": "compaction", "compact_threshold": 200_000}])
    );
    assert!(sent[1].get("context_management").is_none());
    assert!(
        !adapter.compacts_natively(),
        "the harness compacts from now on"
    );
}
