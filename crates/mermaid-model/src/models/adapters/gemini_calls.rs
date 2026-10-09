//! Gemini end to end against a [`MockProvider`]: thought signatures and the
//! computer use tool.
//!
//! What must hold: a function call's signature goes back on that call; with
//! `[tools] computer`, Gemini's computer tool stands in for Mermaid's schema,
//! a computer call reaches the reducer as one `batch` of Mermaid's
//! `computer` tool, and it goes back as written, answered with the
//! screenshot; a model that refuses the tool is retried with Mermaid's
//! schema and remembered.

use serde_json::{Value, json};

use super::gemini::GeminiAdapter;
use super::mock_http::{MockProvider, Reply};
use crate::models::ModelError;
use crate::models::config::{ModelConfig, NativeTools};
use crate::models::stream::StreamEvent;
use crate::models::traits::Model;
use crate::models::types::{ChatMessage, ModelResponse};

const SSE: &str = "text/event-stream";
const MODEL: &str = "gemini-3.8-flash";
const STREAM: &str = ":streamGenerateContent";

fn fixture(name: &str) -> String {
    let path = format!(
        "{}/tests/fixtures/streams/gemini/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn adapter(provider: &MockProvider) -> GeminiAdapter {
    GeminiAdapter::new(
        "key".into(),
        MODEL.into(),
        format!("{}/v1beta", provider.url),
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
        tools: ["read_file", "computer"].into_iter().map(tool).collect(),
        native_tools: NativeTools {
            text_editor: false,
            shell: false,
            computer: true,
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

fn declared(body: &Value) -> Vec<String> {
    body["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .flat_map(|group| -> Vec<String> {
            match group["functionDeclarations"].as_array() {
                Some(list) => list
                    .iter()
                    .map(|f| f["name"].as_str().unwrap_or_default().to_string())
                    .collect(),
                None => group
                    .as_object()
                    .map(|o| o.keys().cloned().collect())
                    .unwrap_or_default(),
            }
        })
        .collect()
}

#[tokio::test]
async fn a_computer_call_runs_as_one_batch_and_goes_back_with_its_screenshot() {
    let provider = MockProvider::start(|_| Reply::stream(SSE, &fixture("computer_call.sse"))).await;
    let adapter = adapter(&provider);
    let user = ChatMessage::user("set the port to 8080");
    let (result, events) = chat(&adapter, std::slice::from_ref(&user), &config()).await;
    let response = result.expect("chat");
    let sent = provider.bodies_to(STREAM);
    assert_eq!(declared(&sent[0]), ["read_file", "computer_use"]);
    assert_eq!(
        sent[0]["tools"][1]["computer_use"]["environment"],
        "ENVIRONMENT_DESKTOP"
    );

    let calls: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ToolCall(call) => Some(call.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].function.name, "computer");
    assert_eq!(
        calls[0].function.arguments,
        json!({"action": "batch", "scale": 1000, "warnings": ["Changes a setting."],
               "actions": [{"action": "left_click", "coordinate": [500, 250]}]})
    );

    let mut turn = ChatMessage::assistant("");
    turn.tool_calls = response.tool_calls.clone();
    let turn = turn.with_provider_continuation(response.provider_continuation.expect("state"));
    let history = [
        user,
        turn,
        ChatMessage::tool("call_0", "computer", "Ran 1 action.")
            .with_images(vec!["iVBORw0KGgoAAAANSUhEUg==".to_string()]),
    ];
    let (result, _) = chat(&adapter, &history, &config()).await;
    result.expect("second turn");
    let contents = provider.bodies_to(STREAM)[1]["contents"].clone();
    assert_eq!(
        contents[1]["parts"][0],
        json!({
            "functionCall": {"name": "click", "args": {
                "x": 500, "y": 250, "intent": "Focus the port field",
                "safety_decision": {"decision": "require_confirmation",
                                    "explanation": "Changes a setting."},
            }},
            "thoughtSignature": "c2lnLWNsaWNr",
        })
    );
    assert_eq!(
        contents[2]["parts"],
        json!([{"functionResponse": {
            "name": "click",
            "response": {"result": "Ran 1 action.", "safety_acknowledgement": "true"},
            "parts": [{"inlineData": {"mimeType": "image/png", "data": "iVBORw0KGgoAAAANSUhEUg=="}}],
        }}]),
        "the screenshot rides in the response, not again after it"
    );

    // A turn without the native tool sends Mermaid's call and plain results.
    let mut plain = config();
    plain.native_tools = NativeTools::default();
    let (result, _) = chat(&adapter, &history, &plain).await;
    result.expect("third turn");
    let sent = provider.bodies_to(STREAM);
    assert_eq!(declared(&sent[2]), ["read_file", "computer"]);
}

#[tokio::test]
async fn a_model_without_the_computer_tool_gets_mermaids_schema_from_then_on() {
    let provider = MockProvider::start(|r| {
        let native = r.body["tools"]
            .as_array()
            .is_some_and(|tools| tools.iter().any(|t| t.get("computer_use").is_some()));
        if native {
            return Reply::error(
                400,
                &json!({"error": {
                    "code": 400,
                    "message": "Computer Use is not enabled for models/gemini-3.8-flash.",
                    "status": "INVALID_ARGUMENT",
                }}),
            );
        }
        Reply::stream(SSE, &fixture("text.sse"))
    })
    .await;
    let adapter = adapter(&provider);
    let (result, _) = chat(&adapter, &[ChatMessage::user("hi")], &config()).await;
    result.expect("retry succeeds");
    let sent = provider.bodies_to(STREAM);
    assert_eq!(sent.len(), 2);
    assert_eq!(declared(&sent[1]), ["read_file", "computer"]);
    assert!(
        adapter
            .param_memory()
            .snapshot()
            .contains("native_computer")
    );
}
