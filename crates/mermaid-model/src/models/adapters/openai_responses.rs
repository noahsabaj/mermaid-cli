//! OpenAI on the Responses API.
//!
//! On Chat Completions, OpenAI's reasoning models lose their reasoning at
//! every tool call: the endpoint neither returns it nor takes it back, so the
//! model thinks again from zero each step. On Responses it comes back as an
//! encrypted item that Mermaid replays (see `responses`), and two more things
//! open up that only exist there:
//!
//! - **OpenAI's own `apply_patch` tool**, the definition GPT-5 models are
//!   trained on. It stands in for Mermaid's `apply_patch`, whose envelope it
//!   already writes, so each native call is rewritten onto that tool as it
//!   arrives and the policy gate, the read-only sandbox, checkpoints and
//!   approvals all see the tool they know. The call itself is replayed in the
//!   native form from the turn's continuation.
//! - **Server-side compaction** (`context_management`): past the trigger the
//!   API summarizes the conversation into an encrypted `compaction` item,
//!   which is replayed in place of everything before it.
//!
//! OpenAI's `shell` tool is not offered. It runs a list of commands and wants
//! an exit outcome per command, which Mermaid's `execute_command` (one
//! command, one result, with a timeout and background mode) cannot report
//! faithfully.
//!
//! The adapter is still [`super::openai_compat::OpenAICompatAdapter`]: the
//! profile says which endpoint a provider speaks, and only OpenAI's says
//! Responses. Every other OpenAI-compatible provider keeps Chat Completions.

use serde_json::{Value, json};

use super::learning::{Optional, Rejections};
use super::responses::{
    APPLY_PATCH, Replay, accepted_effort, combined_instructions, function_tools, messages_to_input,
    sent_effort,
};
use crate::models::config::ModelConfig;
use crate::models::reasoning::{ReasoningCapability, ReasoningLevel, nearest_effort};
use crate::models::types::{ChatMessage, ProviderContinuation};

/// The request field that turns on server-side compaction, and what its
/// refusal is remembered as.
pub(super) const COMPACTION_PARAM: &str = "context_management";
/// The API's floor for `compact_threshold`.
const MIN_COMPACT_THRESHOLD: usize = 1_000;
/// What a refusal of the native `apply_patch` tool is remembered as.
const NATIVE_PATCH: &str = "native_tools";
/// What a refusal of reasoning summaries, and of encrypted reasoning, is
/// remembered as.
const SUMMARY: &str = "reasoning_summary";
const ENCRYPTED: &str = "encrypted_reasoning";

/// The `/responses` request body for `model_name`, avoiding what OpenAI
/// already rejected for this model.
pub(super) fn build_request_body(
    messages: &[ChatMessage],
    config: &ModelConfig,
    model_name: &str,
    reasoning: &ReasoningCapability,
    rejected: &Rejections,
) -> Value {
    let mut body = json!({
        "model": model_name,
        "input": input(messages, model_name),
        "stream": true,
        "store": false,
    });
    if !rejected.contains(ENCRYPTED) {
        body["include"] = json!(["reasoning.encrypted_content"]);
    }
    let mut reasoning_params = serde_json::Map::new();
    if let Some(effort) = accepted_effort(openai_effort(config.reasoning, reasoning), rejected) {
        reasoning_params.insert("effort".to_string(), json!(effort));
    }
    if !rejected.contains(SUMMARY) {
        reasoning_params.insert("summary".to_string(), json!("auto"));
    }
    if !reasoning_params.is_empty() {
        body["reasoning"] = Value::Object(reasoning_params);
    }
    // Same rule as Chat Completions: the o-series and gpt-5 reasoning models
    // reject any non-default temperature, so the catalog hint skips it.
    if crate::models::catalog::lookup(model_name).supports_temperature
        && !rejected.contains("temperature")
    {
        body["temperature"] = json!(config.temperature.clamp(0.0, 2.0));
    }
    let instructions = combined_instructions(config);
    if !instructions.is_empty() {
        body["instructions"] = Value::String(instructions);
    }
    let native_patch = config.native_tools.text_editor
        && !rejected.contains(NATIVE_PATCH)
        && config.tools.iter().any(|tool| {
            tool.pointer("/function/name").and_then(Value::as_str) == Some(APPLY_PATCH)
        });
    let mut tools = function_tools(&config.tools, |name| native_patch && name == APPLY_PATCH);
    if native_patch {
        tools.push(json!({"type": "apply_patch"}));
    }
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    if config.max_tokens > 0 {
        let limit = config
            .resolved_max_output
            .map_or(config.max_tokens, |max| config.max_tokens.min(max));
        body["max_output_tokens"] = json!(limit);
    }
    if let Some(compaction) = config.native_compaction
        && !rejected.contains(COMPACTION_PARAM)
    {
        body[COMPACTION_PARAM] = json!([{
            "type": "compaction",
            "compact_threshold": compaction.trigger_tokens.max(MIN_COMPACT_THRESHOLD),
        }]);
    }
    // `--output-schema` formatting turn: native structured output, with
    // `strict: false` for the same reason as on Chat Completions (strict mode
    // rejects many hand-written schemas; client-side validation is the gate).
    if let Some(schema) = &config.output_schema {
        body["text"] = json!({
            "format": {
                "type": "json_schema",
                "name": "output",
                "strict": false,
                "schema": schema,
            }
        });
    }
    body
}

/// The conversation as input items. Once the API has compacted it, the latest
/// `compaction` item carries everything before it, so the items before it
/// are left out.
fn input(messages: &[ChatMessage], model_name: &str) -> Vec<Value> {
    let mut input = messages_to_input(
        messages,
        |continuation: &ProviderContinuation| continuation.openai_output(model_name),
        Replay { strip_ids: true },
    );
    if let Some(start) = input
        .iter()
        .rposition(|item| item.get("type").and_then(Value::as_str) == Some("compaction"))
    {
        input.drain(..start);
    }
    input
}

/// The effort tier for `level`, snapped onto what the model advertises. OpenAI
/// has no `max`: it collapses to `high`, as on Chat Completions, and the top
/// tier is `xhigh`.
fn openai_effort(level: ReasoningLevel, reasoning: &ReasoningCapability) -> &'static str {
    let level = match reasoning {
        ReasoningCapability::Levels(supported) => {
            nearest_effort(level, supported).unwrap_or(ReasoningLevel::None)
        },
        _ => level,
    };
    match level {
        ReasoningLevel::None => "none",
        ReasoningLevel::Minimal => "minimal",
        ReasoningLevel::Low => "low",
        ReasoningLevel::Medium => "medium",
        ReasoningLevel::High | ReasoningLevel::Max => "high",
        ReasoningLevel::XHigh => "xhigh",
    }
}

/// The optional items a built request carries, for blaming a rejection.
/// Read back off the body so it can't drift from what was actually sent.
pub(super) fn sent_optionals(body: &Value) -> Vec<Optional> {
    let mut sent = Vec::new();
    if body.get("temperature").is_some() {
        sent.push(Optional::new(
            "temperature",
            "temperature",
            &["temperature"],
        ));
    }
    sent.extend(sent_effort(body));
    if body.pointer("/reasoning/summary").is_some() {
        sent.push(Optional::new(
            SUMMARY,
            "reasoning summaries",
            &["summary", "summaries"],
        ));
    }
    if body.get("include").is_some() {
        sent.push(Optional::new(
            ENCRYPTED,
            "encrypted reasoning",
            &["encrypted", "include"],
        ));
    }
    let tools = body.get("tools").and_then(Value::as_array);
    if tools.is_some_and(|tools| tools.iter().any(|tool| tool["type"] == "apply_patch")) {
        // A call already in history names the tool too; that error is about
        // the history, not about offering the tool.
        sent.push(
            Optional::new(NATIVE_PATCH, "OpenAI's apply_patch tool", &["apply_patch"])
                .unless(&["apply_patch_call"]),
        );
    }
    if body.get(COMPACTION_PARAM).is_some() {
        sent.push(Optional::new(
            COMPACTION_PARAM,
            "server-side compaction",
            &[COMPACTION_PARAM, "compact_threshold", "compaction"],
        ));
    }
    sent
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::config::{NativeCompaction, NativeTools};
    use crate::models::types::ResponseItem;

    const MODEL: &str = "gpt-5.5";

    fn levels() -> ReasoningCapability {
        ReasoningCapability::Levels(vec![
            ReasoningLevel::None,
            ReasoningLevel::Minimal,
            ReasoningLevel::Low,
            ReasoningLevel::Medium,
            ReasoningLevel::High,
            ReasoningLevel::Max,
            ReasoningLevel::XHigh,
        ])
    }

    fn tool(name: &str) -> Value {
        json!({"type": "function", "function": {
            "name": name,
            "description": format!("{name} tool"),
            "parameters": {"type": "object"},
        }})
    }

    fn config() -> ModelConfig {
        ModelConfig {
            model: format!("openai/{MODEL}"),
            reasoning: ReasoningLevel::High,
            system_prompt: Some("system".to_string()),
            dynamic_system_suffix: Some("project".to_string()),
            tools: vec![tool("read_file"), tool("apply_patch")],
            native_tools: NativeTools {
                text_editor: true,
                shell: true,
                computer: false,
            },
            ..ModelConfig::default()
        }
    }

    fn body_with(config: &ModelConfig, rejected: &Rejections) -> Value {
        build_request_body(
            &[ChatMessage::user("hello")],
            config,
            MODEL,
            &levels(),
            rejected,
        )
    }

    fn body(config: &ModelConfig) -> Value {
        body_with(config, &Rejections::new())
    }

    #[test]
    fn request_keeps_reasoning_across_turns_without_server_state() {
        let body = body(&config());
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(
            body["reasoning"],
            json!({"effort": "high", "summary": "auto"})
        );
        assert_eq!(body["instructions"], "system\n\nproject");
        assert!(
            body.get("temperature").is_none(),
            "gpt-5 rejects temperature"
        );
        assert!(body.get("previous_response_id").is_none());
        assert!(body.get(COMPACTION_PARAM).is_none());
    }

    #[test]
    fn effort_tiers_follow_chat_completions() {
        for (level, tier) in [
            (ReasoningLevel::None, "none"),
            (ReasoningLevel::Minimal, "minimal"),
            (ReasoningLevel::XHigh, "xhigh"),
            (ReasoningLevel::Max, "high"),
        ] {
            let cfg = ModelConfig {
                reasoning: level,
                ..config()
            };
            assert_eq!(body(&cfg)["reasoning"]["effort"], tier, "{level:?}");
        }
    }

    #[test]
    fn native_apply_patch_replaces_mermaids_schema() {
        let body = body(&config());
        assert_eq!(
            body["tools"],
            json!([
                {"type": "function", "name": "read_file",
                 "description": "read_file tool", "parameters": {"type": "object"}},
                {"type": "apply_patch"},
            ])
        );
    }

    #[test]
    fn mermaids_patch_schema_goes_out_when_native_is_off_refused_or_unregistered() {
        let names = |body: &Value| -> Vec<String> {
            body["tools"]
                .as_array()
                .expect("tools")
                .iter()
                .map(|t| t["name"].as_str().unwrap_or("<native>").to_string())
                .collect()
        };
        let off = ModelConfig {
            native_tools: NativeTools::default(),
            ..config()
        };
        assert_eq!(names(&body(&off)), ["read_file", "apply_patch"]);
        let refused = body_with(&config(), &Rejections::from([NATIVE_PATCH]));
        assert_eq!(names(&refused), ["read_file", "apply_patch"]);
        let read_only = ModelConfig {
            tools: vec![tool("read_file")],
            ..config()
        };
        assert_eq!(names(&body(&read_only)), ["read_file"]);
    }

    #[test]
    fn rejected_reasoning_parameters_are_dropped_one_by_one() {
        let rejected = Rejections::from(["effort", SUMMARY, ENCRYPTED, "temperature"]);
        let body = body_with(&config(), &rejected);
        assert!(body.get("reasoning").is_none());
        assert!(body.get("include").is_none());
        assert!(body.get("temperature").is_none());
        let summary_only = body_with(&config(), &Rejections::from(["effort"]));
        assert_eq!(summary_only["reasoning"], json!({"summary": "auto"}));
    }

    #[test]
    fn non_reasoning_models_get_temperature() {
        let cfg = ModelConfig {
            temperature: 0.3,
            ..config()
        };
        let body = build_request_body(
            &[ChatMessage::user("hi")],
            &cfg,
            "gpt-4.1",
            &levels(),
            &Rejections::new(),
        );
        assert!((body["temperature"].as_f64().expect("temperature") - 0.3).abs() < 1e-6);
    }

    #[test]
    fn server_compaction_is_asked_for_and_floored() {
        let cfg = ModelConfig {
            native_compaction: Some(NativeCompaction {
                trigger_tokens: 340_000,
            }),
            ..config()
        };
        assert_eq!(
            body(&cfg)[COMPACTION_PARAM],
            json!([{"type": "compaction", "compact_threshold": 340_000}])
        );
        let tiny = ModelConfig {
            native_compaction: Some(NativeCompaction { trigger_tokens: 10 }),
            ..config()
        };
        assert_eq!(
            body(&tiny)[COMPACTION_PARAM][0]["compact_threshold"],
            MIN_COMPACT_THRESHOLD
        );
        let refused = body_with(&cfg, &Rejections::from([COMPACTION_PARAM]));
        assert!(refused.get(COMPACTION_PARAM).is_none());
    }

    #[test]
    fn output_schema_maps_to_text_format() {
        let cfg = ModelConfig {
            output_schema: Some(json!({"type": "object"})),
            ..config()
        };
        assert_eq!(
            body(&cfg)["text"],
            json!({"format": {
                "type": "json_schema", "name": "output", "strict": false,
                "schema": {"type": "object"},
            }})
        );
    }

    fn openai_turn(model: &str, output: Vec<Value>) -> ChatMessage {
        ChatMessage::assistant("").with_provider_continuation(
            ProviderContinuation::OpenaiResponses {
                model: model.to_string(),
                output: output.into_iter().map(ResponseItem::from_wire).collect(),
            },
        )
    }

    #[test]
    fn history_before_the_latest_compaction_is_left_out() {
        let history = [
            ChatMessage::user("long ago"),
            openai_turn(
                MODEL,
                vec![
                    json!({"type": "compaction", "id": "cmp_1", "encrypted_content": "summary"}),
                    json!({"type": "message", "id": "msg_1", "role": "assistant",
                           "content": [{"type": "output_text", "text": "done"}]}),
                ],
            ),
            ChatMessage::user("next"),
        ];
        let input = input(&history, MODEL);
        assert_eq!(input.len(), 3);
        assert_eq!(input[0]["type"], "compaction");
        assert_eq!(input[0]["encrypted_content"], "summary");
        assert!(input[1].get("id").is_none(), "store is false: no item ids");
        assert_eq!(input[2]["content"][0]["text"], "next");
    }

    #[test]
    fn another_models_state_is_replayed_as_plain_history() {
        let mut turn = openai_turn(
            "gpt-5-mini",
            vec![
                json!({"type": "reasoning", "id": "rs_1", "summary": [],
                       "encrypted_content": "theirs"}),
                json!({"type": "compaction", "encrypted_content": "theirs too"}),
            ],
        );
        turn.content = "answer".to_string();
        let history = [ChatMessage::user("question"), turn];
        let input = input(&history, MODEL);
        assert_eq!(input.len(), 2, "nothing encrypted, nothing dropped");
        assert_eq!(input[1]["role"], "assistant");
        assert_eq!(input[1]["content"][0]["text"], "answer");
    }

    #[test]
    fn sent_optionals_read_the_body_back() {
        let cfg = ModelConfig {
            temperature: 0.2,
            native_compaction: Some(NativeCompaction {
                trigger_tokens: 5_000,
            }),
            ..config()
        };
        let body = build_request_body(
            &[ChatMessage::user("hi")],
            &cfg,
            "gpt-4.1",
            &levels(),
            &Rejections::new(),
        );
        let remembered: Vec<String> = sent_optionals(&body)
            .into_iter()
            .map(|o| o.remember)
            .collect();
        assert_eq!(
            remembered,
            [
                "temperature",
                "effort",
                SUMMARY,
                ENCRYPTED,
                NATIVE_PATCH,
                COMPACTION_PARAM
            ]
        );
    }
}
