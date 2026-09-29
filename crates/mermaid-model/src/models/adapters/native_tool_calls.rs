//! Anthropic's own tools, driven end to end against a [`MockProvider`].
//!
//! Four things must hold: a turn that allows them declares them in place of
//! Mermaid's schemas for the same tools; a native call reaches the reducer as
//! the Mermaid tool it stands for, with its id noted on the continuation; the
//! next request sends it back in the native form; and a model that refuses
//! them is retried with Mermaid's schemas and remembered.

use serde_json::{Value, json};

use super::anthropic::AnthropicAdapter;
use super::mock_http::{MockProvider, Reply};
use crate::models::ModelError;
use crate::models::config::{ModelConfig, NativeTools};
use crate::models::stream::StreamEvent;
use crate::models::traits::Model;
use crate::models::types::{ChatMessage, ModelResponse};

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

fn tool(name: &str) -> Value {
    json!({"type": "function", "function": {
        "name": name,
        "description": format!("{name} tool"),
        "parameters": {"type": "object", "properties": {}},
    }})
}

fn config(native: NativeTools) -> ModelConfig {
    ModelConfig {
        tools: [
            "read_file",
            "write_file",
            "edit_file",
            "apply_patch",
            "execute_command",
        ]
        .into_iter()
        .map(tool)
        .collect(),
        native_tools: native,
        ..ModelConfig::default()
    }
}

const BOTH: NativeTools = NativeTools {
    text_editor: true,
    shell: true,
};

fn tool_names(body: &Value) -> Vec<String> {
    body["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|t| t["name"].as_str().expect("name").to_string())
        .collect()
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
async fn native_tools_replace_the_schemas_they_stand_for() {
    let provider = MockProvider::start(|_| Reply::stream(SSE, &fixture("text.sse"))).await;
    let adapter = adapter(&provider);
    let (result, _) = chat(&adapter, &[ChatMessage::user("hi")], &config(BOTH)).await;
    result.expect("chat");

    let body = &provider.bodies_to("/messages")[0];
    assert_eq!(
        tool_names(body),
        [
            "apply_patch",
            "execute_command",
            "str_replace_based_edit_tool",
            "bash"
        ]
    );
    let tools = body["tools"].as_array().expect("tools");
    assert_eq!(tools[2]["type"], "text_editor_20250728");
    assert_eq!(tools[3]["type"], "bash_20250124");
    assert!(tools[2].get("input_schema").is_none(), "schema-less");
    assert!(
        tools[3].get("cache_control").is_some(),
        "the cache marker stays on the last tool"
    );
}

#[tokio::test]
async fn a_turn_that_does_not_allow_them_sends_mermaids_schemas() {
    let provider = MockProvider::start(|_| Reply::stream(SSE, &fixture("text.sse"))).await;
    let adapter = adapter(&provider);
    let (result, _) = chat(
        &adapter,
        &[ChatMessage::user("hi")],
        &config(NativeTools::default()),
    )
    .await;
    result.expect("chat");
    let body = &provider.bodies_to("/messages")[0];
    assert_eq!(
        tool_names(body),
        [
            "read_file",
            "write_file",
            "edit_file",
            "apply_patch",
            "execute_command"
        ]
    );
}

#[tokio::test]
async fn native_calls_reach_the_harness_as_its_own_tools_and_go_back_as_written() {
    let provider =
        MockProvider::start(|_| Reply::stream(SSE, &fixture("native_tool_calls.sse"))).await;
    let adapter = adapter(&provider);
    let (result, events) = chat(&adapter, &[ChatMessage::user("fix it")], &config(BOTH)).await;
    let response = result.expect("chat");

    // What the reducer (and so every gate) sees.
    let streamed: Vec<(String, Value)> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ToolCall(tc) => {
                Some((tc.function.name.clone(), tc.function.arguments.clone()))
            },
            _ => None,
        })
        .collect();
    assert_eq!(
        streamed,
        [
            (
                "edit_file".to_string(),
                json!({"path": "src/lib.rs", "target_content": "old", "replacement_content": "new"})
            ),
            (
                "execute_command".to_string(),
                json!({"command": "cargo test", "timeout": 300})
            ),
            ("read_file".to_string(), json!({"path": "a.txt"})),
        ]
    );
    let calls = response.tool_calls.clone().expect("tool calls");
    assert_eq!(calls[0].function.name, "edit_file");
    let continuation = response
        .provider_continuation
        .clone()
        .expect("continuation notes the native calls");
    assert!(continuation.is_anthropic_native_call("toolu_edit"));
    assert!(continuation.is_anthropic_native_call("toolu_bash"));
    assert!(!continuation.is_anthropic_native_call("toolu_read"));

    // The next request replays the native calls natively and the Mermaid call
    // as it was.
    let mut assistant = ChatMessage::assistant("").with_provider_continuation(continuation);
    assistant.tool_calls = Some(calls);
    let history = vec![
        ChatMessage::user("fix it"),
        assistant,
        ChatMessage::tool("toolu_edit", "edit_file", "Edited src/lib.rs"),
        ChatMessage::tool("toolu_bash", "execute_command", "ok"),
        ChatMessage::tool("toolu_read", "read_file", "a"),
    ];
    let (result, _) = chat(&adapter, &history, &config(BOTH)).await;
    result.expect("second chat");
    let sent = provider.bodies_to("/messages");
    let replayed = &sent[1]["messages"][1]["content"];
    assert_eq!(
        replayed[0],
        json!({"type": "tool_use", "id": "toolu_edit", "name": "str_replace_based_edit_tool",
               "input": {"command": "str_replace", "path": "src/lib.rs", "old_str": "old", "new_str": "new"}})
    );
    assert_eq!(
        replayed[1],
        json!({"type": "tool_use", "id": "toolu_bash", "name": "bash",
               "input": {"command": "cargo test"}})
    );
    assert_eq!(replayed[2]["name"], "read_file");

    // A later turn without the native tools sends Mermaid's names throughout.
    let (result, _) = chat(&adapter, &history, &config(NativeTools::default())).await;
    result.expect("third chat");
    let sent = provider.bodies_to("/messages");
    let replayed = &sent[2]["messages"][1]["content"];
    assert_eq!(replayed[0]["name"], "edit_file");
    assert_eq!(replayed[1]["name"], "execute_command");
}

#[tokio::test]
async fn a_model_that_refuses_them_is_retried_with_mermaids_schemas_and_remembered() {
    let provider = MockProvider::start(|r| {
        let native = r.body["tools"]
            .as_array()
            .is_some_and(|tools| tools.iter().any(|t| t["type"] != "custom"));
        if native {
            return Reply::error(
                400,
                &json!({"type": "error", "error": {
                    "type": "invalid_request_error",
                    "message": "tools.5: Input tag 'text_editor_20250728' found using 'type' does not match any of the expected tags",
                }}),
            );
        }
        Reply::stream(SSE, &fixture("text.sse"))
    })
    .await;
    let adapter = adapter(&provider);

    let (result, events) = chat(&adapter, &[ChatMessage::user("hi")], &config(BOTH)).await;
    assert_eq!(result.expect("retry succeeds").content, "Hello, world");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, StreamEvent::Status(s) if s.contains("text editor"))),
        "the user hears what was taken back"
    );
    let sent = provider.bodies_to("/messages");
    assert_eq!(sent.len(), 2);
    assert!(tool_names(&sent[1]).contains(&"read_file".to_string()));

    // Remembered: the next turn goes straight to Mermaid's schemas.
    let (result, _) = chat(&adapter, &[ChatMessage::user("hi")], &config(BOTH)).await;
    result.expect("third call");
    let sent = provider.bodies_to("/messages");
    assert_eq!(sent.len(), 3);
    assert!(!tool_names(&sent[2]).contains(&"bash".to_string()));
}
