//! The Responses wire format, shared by the Meta and OpenAI adapters.
//!
//! Both speak `POST /responses` the same way: stateless replay. Every request
//! sets `store: false` and asks for `reasoning.encrypted_content`; the
//! returned output items are persisted on the assistant message and replayed
//! verbatim next turn, so the model's reasoning survives each tool call.
//! What differs per provider (the request parameters, the reasoning tiers,
//! the native tools) stays in each adapter.

use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};

use super::accumulator::{CappedText, parse_tool_args, slot_in_bounds};
use super::learning::{Optional, Rejections};
use crate::models::adapters::driver::{Flow, Framing, StreamProtocol};
use crate::models::config::ModelConfig;
use crate::models::error::{BackendError, ModelError, Result};
use crate::models::reasoning::ReasoningChunk;
use crate::models::stream::StreamEvent;
use crate::models::tool_call::{FunctionCall, ToolCall};
use crate::models::types::{
    ChatMessage, FinishReason, MessageRole, ModelResponse, ProviderContinuation, ResponseItem,
    TokenUsage,
};

/// Which provider a stream or request belongs to: it names the provider in
/// errors and decides which continuation the output items are saved under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Provider {
    Meta,
    OpenAi,
}

impl Provider {
    /// How errors name the provider.
    const fn label(self) -> &'static str {
        match self {
            Self::Meta => "Meta",
            Self::OpenAi => "OpenAI",
        }
    }

    /// The backend name structured errors carry.
    const fn backend(self) -> &'static str {
        match self {
            Self::Meta => "meta",
            Self::OpenAi => "openai",
        }
    }

    fn continuation(self, model: &str, output: Vec<ResponseItem>) -> ProviderContinuation {
        match self {
            Self::Meta => ProviderContinuation::MetaResponses { output },
            Self::OpenAi => ProviderContinuation::OpenaiResponses {
                model: model.to_string(),
                output,
            },
        }
    }
}

/// A Responses event stream as a [`StreamProtocol`].
///
/// The terminal frame is explicit (`response.completed` / `.incomplete`) and
/// carries the whole response object, so unlike the other adapters this
/// protocol accumulates almost nothing during the stream: the text deltas are
/// for display, and the authoritative output items arrive at the end.
pub(crate) struct ResponsesStream {
    provider: Provider,
    model_name: String,
    /// Tool calls already emitted, by `call_id`. The same call arrives twice
    /// (once as `response.output_item.done`, once in the terminal response's
    /// `output`) and the agent must not run it twice.
    emitted_calls: HashSet<String>,
    tool_calls: Vec<ToolCall>,
    content: CappedText,
    thinking: CappedText,
    /// The terminal frame's `response` object plus which event carried it.
    /// `None` until then, which is exactly what makes a cut body detectable.
    terminal: Option<(Value, String)>,
}

impl ResponsesStream {
    pub(crate) fn new(provider: Provider, model_name: String) -> Self {
        Self {
            provider,
            model_name,
            emitted_calls: HashSet::new(),
            tool_calls: Vec::new(),
            content: CappedText::default(),
            thinking: CappedText::default(),
            terminal: None,
        }
    }

    /// Record and emit a tool-call output item, once per `call_id`.
    fn take_tool_call(&mut self, item: &Value, out: &mut Vec<StreamEvent>) {
        let Some(call) = tool_call_from_item(item) else {
            return;
        };
        let call_id = call.id.clone().unwrap_or_default();
        if !slot_in_bounds(self.tool_calls.len()) {
            // A stream can mint a fresh call_id per frame forever; past the
            // bound the call is dropped rather than accumulated.
            tracing::warn!(
                provider = self.provider.backend(),
                "Responses stream exceeded the tool-call bound; ignoring further calls"
            );
            return;
        }
        if self.emitted_calls.insert(call_id) {
            self.tool_calls.push(call.clone());
            out.push(StreamEvent::ToolCall(call));
        }
    }
}

impl StreamProtocol for ResponsesStream {
    const FRAMING: Framing = Framing::Sse;

    fn on_frame(&mut self, frame: &str, out: &mut Vec<StreamEvent>) -> Result<Flow> {
        let event: Value = serde_json::from_str(frame).map_err(|error| ModelError::ParseError {
            message: format!(
                "failed to parse {} Responses event: {error}",
                self.provider.label()
            ),
            raw: None,
        })?;
        let event_type = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match event_type {
            "response.output_text.delta" => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str)
                    && self.content.accepting()
                {
                    self.content.push(delta);
                    out.push(StreamEvent::Text(delta.to_string()));
                }
            },
            "response.reasoning_summary_text.delta" => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str)
                    && self.thinking.accepting()
                {
                    self.thinking.push(delta);
                    out.push(StreamEvent::Reasoning(ReasoningChunk {
                        text: delta.to_string(),
                        signature: None,
                    }));
                }
            },
            "response.output_item.done" => {
                if let Some(item) = event.get("item") {
                    self.take_tool_call(item, out);
                }
            },
            "response.completed" | "response.incomplete" => {
                let response = event
                    .get("response")
                    .ok_or_else(|| ModelError::ParseError {
                        message: format!(
                            "{} {event_type} event omitted response",
                            self.provider.label()
                        ),
                        raw: None,
                    })?
                    .clone();
                // The terminal `output` repeats every item, including calls
                // already streamed; `take_tool_call` dedupes by `call_id`.
                for item in response
                    .get("output")
                    .and_then(Value::as_array)
                    .unwrap_or(&Vec::new())
                {
                    self.take_tool_call(item, out);
                }
                self.terminal = Some((response, event_type.to_string()));
                return Ok(Flow::Stop);
            },
            "response.failed" | "error" => return Err(failure(self.provider, &event)),
            "response.cancelled" => {
                return Err(ModelError::StreamError(format!(
                    "{} cancelled the response",
                    self.provider.label()
                )));
            },
            _ => {},
        }
        Ok(Flow::Continue)
    }

    fn finish(self, _out: &mut Vec<StreamEvent>) -> Result<ModelResponse> {
        // Responses spelling: the terminal event is explicit and carries
        // everything that round-trips, so its absence means the connection
        // dropped, never a short success.
        let Some((response, event_type)) = self.terminal else {
            return Err(ModelError::StreamError(format!(
                "{} Responses stream closed before a terminal event",
                self.provider.label()
            )));
        };

        let output = response
            .get("output")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let continuation = self.provider.continuation(
            &self.model_name,
            output
                .into_iter()
                .filter(item_is_replayable)
                .map(ResponseItem::from_wire)
                .collect(),
        );
        let stop_reason = finish_reason(&response, &event_type, !self.tool_calls.is_empty());

        Ok(ModelResponse {
            content: self.content.into_string(),
            usage: response.get("usage").map(usage),
            model_name: self.model_name,
            stop_reason: Some(stop_reason),
            thinking: (!self.thinking.is_empty()).then(|| self.thinking.into_string()),
            tool_calls: (!self.tool_calls.is_empty()).then_some(self.tool_calls),
            provider_continuation: Some(continuation),
        })
    }
}

/// A tool-call output item as the call Mermaid runs. OpenAI's own
/// `apply_patch` tool arrives as an `apply_patch_call` and is rewritten onto
/// Mermaid's `apply_patch` (see [`apply_patch_envelope`]), so every gate sees
/// the tool it knows.
pub(crate) fn tool_call_from_item(item: &Value) -> Option<ToolCall> {
    let call_id = item.get("call_id")?.as_str()?.to_string();
    let function = match item.get("type").and_then(Value::as_str)? {
        "function_call" => {
            let name = item.get("name")?.as_str()?.to_string();
            let raw_arguments = item
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}");
            let arguments = parse_tool_args(&name, raw_arguments.to_string());
            FunctionCall { name, arguments }
        },
        "apply_patch_call" => {
            let operation = item.get("operation").cloned().unwrap_or(Value::Null);
            // An operation with no envelope still runs, as a patch Mermaid's
            // tool refuses: the API wants an output for every call it made,
            // and the refusal tells the model why.
            let arguments = match apply_patch_envelope(&operation) {
                Some(patch) => json!({"patch": patch}),
                None => json!({"operation": operation}),
            };
            FunctionCall {
                name: APPLY_PATCH.to_string(),
                arguments,
            }
        },
        _ => return None,
    };
    Some(ToolCall {
        id: Some(call_id),
        function,
    })
}

/// Mermaid's tool that OpenAI's native `apply_patch` stands in for.
pub(crate) const APPLY_PATCH: &str = "apply_patch";

/// One native `apply_patch` operation as the `*** Begin Patch` envelope
/// Mermaid's `apply_patch` takes. The operation's `diff` is already that
/// format's body for one file (`+` lines for a new file, `@@` hunks for an
/// update), so only the file header is added. `None` for an operation with
/// no path or an unknown type.
fn apply_patch_envelope(operation: &Value) -> Option<String> {
    let path = operation.get("path")?.as_str()?;
    let diff = || {
        operation
            .get("diff")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim_end_matches('\n')
    };
    let body = match operation.get("type")?.as_str()? {
        "create_file" => format!("*** Add File: {path}\n{}", diff()),
        "update_file" => format!("*** Update File: {path}\n{}", diff()),
        "delete_file" => format!("*** Delete File: {path}"),
        _ => return None,
    };
    Some(format!("*** Begin Patch\n{body}\n*** End Patch"))
}

/// Unwrap the OpenAI `{"type":"function","function":{...}}` envelope that
/// `ToolDefinition::to_openai_json` produces into the flat shape Responses
/// wants, leaving out the tools `skip` names. Same job `to_anthropic_tools`
/// does for Anthropic.
pub(crate) fn function_tools(openai_tools: &[Value], skip: impl Fn(&str) -> bool) -> Vec<Value> {
    openai_tools
        .iter()
        .filter_map(|tool| {
            let function = tool.get("function")?;
            let name = function.get("name")?;
            if name.as_str().is_some_and(&skip) {
                return None;
            }
            Some(json!({
                "type": "function",
                "name": name,
                "description": function.get("description").cloned().unwrap_or(Value::Null),
                "parameters": function.get("parameters").cloned().unwrap_or_else(|| json!({})),
            }))
        })
        .collect()
}

/// How a provider's own output items go back on the wire.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Replay {
    /// Drop the `id` of every item that carries no encrypted state. Under
    /// `store: false` the server keeps no item, so an id points at nothing;
    /// the `call_id` is what pairs a call with its output.
    pub(crate) strip_ids: bool,
}

/// The conversation as Responses input items. An assistant message whose
/// continuation `replayed` accepts goes back as the output items the provider
/// wrote; every other message is rebuilt from the transcript.
pub(crate) fn messages_to_input<'a>(
    messages: &'a [ChatMessage],
    replayed: impl Fn(&'a ProviderContinuation) -> Option<&'a [ResponseItem]>,
    replay: Replay,
) -> Vec<Value> {
    let mut input = Vec::new();
    // The output item type each natively-made call is answered with, by
    // `call_id`. Anything not here is a function call.
    let mut native_outputs: HashMap<&str, String> = HashMap::new();
    for message in messages {
        if message.role == MessageRole::Assistant
            && let Some(output) = message.provider_continuation.as_ref().and_then(&replayed)
        {
            for item in output {
                if let Some(call_id) = item.call_id()
                    && item.kind() != "function_call"
                {
                    native_outputs.insert(call_id, format!("{}_output", item.kind()));
                }
            }
            input.extend(output_to_input(output, replay));
            continue;
        }
        match message.role {
            MessageRole::Tool => {
                let call_id = message.tool_call_id.as_deref().unwrap_or_default();
                input.push(match native_outputs.get(call_id) {
                    Some(kind) => json!({
                        "type": kind,
                        "call_id": call_id,
                        "status": "completed",
                        "output": message.content,
                    }),
                    None => json!({
                        "type": "function_call_output",
                        "call_id": call_id,
                        "output": message.content,
                    }),
                });
            },
            MessageRole::User => input.push(input_message(message, "user", "input_text")),
            MessageRole::System => input.push(input_message(message, "system", "input_text")),
            MessageRole::Assistant => {
                if !message.content.is_empty() {
                    let mut assistant = input_message(message, "assistant", "output_text");
                    if message
                        .tool_calls
                        .as_ref()
                        .is_some_and(|calls| !calls.is_empty())
                    {
                        assistant["phase"] = json!("commentary");
                    }
                    input.push(assistant);
                }
                for call in message.tool_calls.iter().flatten() {
                    input.push(json!({
                        "type": "function_call",
                        "call_id": call.id.clone().unwrap_or_default(),
                        "name": call.function.name,
                        "arguments": serde_json::to_string(&call.function.arguments)
                            .unwrap_or_else(|_| "{}".to_string()),
                        "status": "completed",
                    }));
                }
            },
        }
    }
    input
}

pub(crate) fn output_to_input(output: &[ResponseItem], replay: Replay) -> Vec<Value> {
    let mut input = output
        .iter()
        .map(|item| {
            let mut wire = item.to_wire();
            if replay.strip_ids
                && matches!(item, ResponseItem::Other { .. })
                && let Some(object) = wire.as_object_mut()
            {
                object.remove("id");
            }
            wire
        })
        .collect::<Vec<_>>();
    // The API rejects a replayed reasoning item followed directly by the next
    // user turn. A rare reasoning-only response therefore needs a minimal
    // assistant message before the conversation continues.
    if input
        .last()
        .and_then(|item| item.get("type"))
        .and_then(Value::as_str)
        == Some("reasoning")
    {
        input.push(json!({
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "I will continue."}]
        }));
    }
    input
}

fn input_message(message: &ChatMessage, role: &str, text_type: &str) -> Value {
    let mut content = Vec::new();
    if !message.content.is_empty() {
        content.push(json!({"type": text_type, "text": message.content}));
    }
    if role == "user" {
        for image in message.images.iter().flatten() {
            content.push(json!({
                "type": "input_image",
                "image_url": format!("data:image/png;base64,{image}"),
            }));
        }
    }
    json!({"type": "message", "role": role, "content": content})
}

/// The static system prompt and the project's `MERMAID.md` suffix, joined the
/// way Responses wants them: one `instructions` string, blank line between.
pub(crate) fn combined_instructions(config: &ModelConfig) -> String {
    let system = config.system_prompt.as_deref().unwrap_or_default();
    match config
        .dynamic_system_suffix
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        Some(suffix) if !system.is_empty() => format!("{system}\n\n{suffix}"),
        Some(suffix) => suffix.to_string(),
        None => system.to_string(),
    }
}

/// Step an effort tier down past what this model rejected: `max` → `xhigh`
/// → `high`, `minimal` → `low`. A rejected base tier (`low`/`medium`/`high`)
/// is remembered as `effort` itself, which omits the field, and so is a
/// rejected `none`: omitting the field is what "no reasoning" meant before
/// `none` existed.
pub(crate) fn accepted_effort(
    mut tier: &'static str,
    rejected: &Rejections,
) -> Option<&'static str> {
    if rejected.contains("effort") {
        return None;
    }
    while rejected.contains(&format!("effort:{tier}")) {
        tier = match tier {
            "max" => "xhigh",
            "xhigh" => "high",
            "minimal" => "low",
            _ => return None,
        };
    }
    Some(tier)
}

/// The effort tier a built request carries, as an item to blame a rejection
/// on. Read back off the body so it can't drift from what was actually sent.
pub(crate) fn sent_effort(body: &Value) -> Option<Optional> {
    let tier = body.pointer("/reasoning/effort").and_then(Value::as_str)?;
    let remember = match tier {
        "max" | "xhigh" | "minimal" | "none" => format!("effort:{tier}"),
        _ => "effort".to_string(),
    };
    Some(Optional::new(
        &remember,
        &format!("reasoning effort \"{tier}\""),
        &["effort"],
    ))
}

fn item_is_replayable(item: &Value) -> bool {
    item.get("type").and_then(Value::as_str) != Some("reasoning")
        || item
            .get("encrypted_content")
            .and_then(Value::as_str)
            .is_some()
}

pub(crate) fn usage(value: &Value) -> TokenUsage {
    let input = usize_field(value, "input_tokens");
    let output = usize_field(value, "output_tokens");
    let cached = value
        .get("input_tokens_details")
        .map(|details| usize_field(details, "cached_tokens"))
        .unwrap_or_default();
    let reasoning = value
        .get("output_tokens_details")
        .map(|details| usize_field(details, "reasoning_tokens"))
        .unwrap_or_default();
    // Responses-API wire counts nest cached inside input_tokens and
    // reasoning inside output_tokens; carve both out so the shared
    // TokenUsage components stay disjoint (matches openai_compat).
    TokenUsage::provider(
        input.saturating_sub(cached),
        output.saturating_sub(reasoning),
    )
    .with_cached_input(cached)
    .with_reasoning_output(reasoning)
}

fn usize_field(value: &Value, key: &str) -> usize {
    value
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or_default()
}

pub(crate) fn finish_reason(response: &Value, event_type: &str, has_tools: bool) -> FinishReason {
    let incomplete_reason = response
        .get("incomplete_details")
        .and_then(|details| details.get("reason"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if event_type == "response.incomplete"
        || response.get("status").and_then(Value::as_str) == Some("incomplete")
    {
        if incomplete_reason.contains("max_output") || incomplete_reason.contains("length") {
            return FinishReason::Length;
        }
        if incomplete_reason.contains("content_filter") || incomplete_reason.contains("safety") {
            return FinishReason::ContentFilter;
        }
        return FinishReason::Other(if incomplete_reason.is_empty() {
            "incomplete".to_string()
        } else {
            incomplete_reason.to_string()
        });
    }
    if has_tools {
        FinishReason::ToolUse
    } else {
        FinishReason::Stop
    }
}

pub(crate) fn failure(provider: Provider, event: &Value) -> ModelError {
    let error = event
        .get("response")
        .and_then(|response| response.get("error"))
        .or_else(|| event.get("error"));
    let fallback = format!("{} Responses request failed", provider.label());
    let message = error
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .or_else(|| event.get("message").and_then(Value::as_str))
        .unwrap_or(&fallback);
    ModelError::Backend(BackendError::ProviderError {
        provider: provider.backend().to_string(),
        code: error
            .and_then(|error| error.get("code"))
            .and_then(Value::as_str)
            .map(str::to_string),
        message: crate::utils::redact_secrets(message),
        debug: crate::models::error::ResponseDebugContext::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const VERBATIM: Replay = Replay { strip_ids: false };

    fn meta_output(continuation: &ProviderContinuation) -> Option<&[ResponseItem]> {
        continuation.meta_output()
    }

    #[test]
    fn continuation_replays_order_phase_and_encrypted_content() {
        let output = vec![
            ResponseItem::from_wire(json!({
                "type": "reasoning",
                "id": "rs_1",
                "summary": [],
                "encrypted_content": "eyJcipher.payload.signature"
            })),
            ResponseItem::from_wire(json!({
                "type": "message",
                "role": "assistant",
                "phase": "commentary",
                "content": [{"type": "output_text", "text": "checking"}]
            })),
            ResponseItem::from_wire(json!({
                "type": "function_call",
                "call_id": "call_1",
                "name": "read_file",
                "arguments": "{\"path\":\"README.md\"}"
            })),
        ];
        let message = ChatMessage::assistant("checking")
            .with_provider_continuation(ProviderContinuation::MetaResponses { output });
        let history = [
            message,
            ChatMessage::tool("call_1", "read_file", "contents"),
        ];
        let input = messages_to_input(&history, meta_output, VERBATIM);
        assert_eq!(input[0]["type"], "reasoning");
        assert_eq!(input[0]["encrypted_content"], "eyJcipher.payload.signature");
        assert_eq!(input[1]["phase"], "commentary");
        assert_eq!(input[2]["call_id"], "call_1");
        assert_eq!(input[3]["type"], "function_call_output");
    }

    #[test]
    fn reasoning_only_replay_gets_required_assistant_follower() {
        let output = vec![ResponseItem::from_wire(json!({
            "type": "reasoning",
            "id": "rs_1",
            "summary": [],
            "encrypted_content": "ciphertext"
        }))];
        let input = output_to_input(&output, VERBATIM);
        assert_eq!(input[0]["type"], "reasoning");
        assert_eq!(input[1]["type"], "message");
        assert_eq!(input[1]["role"], "assistant");
    }

    #[test]
    fn stripping_ids_keeps_them_on_encrypted_items_only() {
        let output = vec![
            ResponseItem::from_wire(json!({
                "type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "c"
            })),
            ResponseItem::from_wire(json!({
                "type": "compaction", "id": "cmp_1", "encrypted_content": "k"
            })),
            ResponseItem::from_wire(json!({
                "type": "function_call", "id": "fc_1", "call_id": "call_1",
                "name": "read_file", "arguments": "{}"
            })),
        ];
        let input = output_to_input(&output, Replay { strip_ids: true });
        assert_eq!(input[0]["id"], "rs_1");
        assert_eq!(input[1]["id"], "cmp_1");
        assert_eq!(input[1]["encrypted_content"], "k");
        assert!(input[2].get("id").is_none());
        assert_eq!(input[2]["call_id"], "call_1");
    }

    #[test]
    fn a_native_patch_call_is_answered_in_kind() {
        let patch = json!({
            "type": "apply_patch_call",
            "call_id": "call_p",
            "status": "completed",
            "operation": {"type": "delete_file", "path": "old.txt"}
        });
        let message = ChatMessage::assistant("").with_provider_continuation(
            ProviderContinuation::MetaResponses {
                output: vec![ResponseItem::from_wire(patch.clone())],
            },
        );
        let history = [
            message,
            ChatMessage::tool("call_p", "apply_patch", "Deleted old.txt"),
        ];
        let input = messages_to_input(&history, meta_output, VERBATIM);
        assert_eq!(input[0], patch, "the call goes back as the model wrote it");
        assert_eq!(
            input[1],
            json!({
                "type": "apply_patch_call_output",
                "call_id": "call_p",
                "status": "completed",
                "output": "Deleted old.txt",
            })
        );
    }

    #[test]
    fn native_patch_operations_become_mermaid_patch_envelopes() {
        let call = |operation: Value| {
            tool_call_from_item(&json!({
                "type": "apply_patch_call",
                "call_id": "call_1",
                "operation": operation,
            }))
            .expect("apply_patch_call parses")
        };
        let update = call(json!({
            "type": "update_file",
            "path": "lib/fib.py",
            "diff": "@@\n-def fib(n):\n+def fibonacci(n):\n     if n <= 1:\n"
        }));
        assert_eq!(update.id.as_deref(), Some("call_1"));
        assert_eq!(update.function.name, "apply_patch");
        assert_eq!(
            update.function.arguments,
            json!({"patch": "*** Begin Patch\n*** Update File: lib/fib.py\n@@\n-def fib(n):\n+def fibonacci(n):\n     if n <= 1:\n*** End Patch"})
        );
        let create = call(json!({"type": "create_file", "path": "a.txt", "diff": "+one\n+two"}));
        assert_eq!(
            create.function.arguments["patch"],
            "*** Begin Patch\n*** Add File: a.txt\n+one\n+two\n*** End Patch"
        );
        let delete = call(json!({"type": "delete_file", "path": "a.txt"}));
        assert_eq!(
            delete.function.arguments["patch"],
            "*** Begin Patch\n*** Delete File: a.txt\n*** End Patch"
        );
        let unknown = call(json!({"type": "rename_file", "path": "a"}));
        assert_eq!(unknown.function.name, "apply_patch");
        assert!(
            unknown.function.arguments.get("patch").is_none(),
            "an operation with no envelope reaches the tool as one it refuses"
        );
    }

    #[test]
    fn parses_tool_calls_usage_and_finish_reasons() {
        let call = tool_call_from_item(&json!({
            "type": "function_call",
            "call_id": "call_7",
            "name": "execute_command",
            "arguments": "{\"cmd\":\"pwd\"}"
        }))
        .expect("function_call parses");
        assert_eq!(call.id.as_deref(), Some("call_7"));
        assert_eq!(call.function.arguments["cmd"], "pwd");

        let usage = usage(&json!({
            "input_tokens": 100,
            "output_tokens": 40,
            "total_tokens": 140,
            "input_tokens_details": {"cached_tokens": 20},
            "output_tokens_details": {"reasoning_tokens": 15}
        }));
        assert_eq!(usage.prompt_tokens, 80, "cached carved out of input");
        assert_eq!(
            usage.completion_tokens, 25,
            "reasoning carved out of output"
        );
        assert_eq!(usage.total_tokens(), 140);
        assert_eq!(usage.cached_input_tokens, 20);
        assert_eq!(usage.reasoning_output_tokens, 15);
        assert_eq!(
            finish_reason(
                &json!({"status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"}}),
                "response.incomplete",
                false,
            ),
            FinishReason::Length
        );
        assert_eq!(
            finish_reason(&json!({}), "response.completed", true),
            FinishReason::ToolUse
        );
    }

    #[test]
    fn failed_event_is_redacted_and_structured() {
        let error = failure(
            Provider::OpenAi,
            &json!({
                "type": "response.failed",
                "response": {
                    "error": {
                        "code": "bad_request",
                        "message": "Authorization: Bearer abcdef123456ghijkl"
                    }
                }
            }),
        );
        let rendered = error.to_string();
        assert!(rendered.contains("bad_request"));
        assert!(rendered.contains("[REDACTED]"));
        assert!(!rendered.contains("abcdef123456ghijkl"));
    }

    #[test]
    fn effort_steps_down_past_rejected_tiers() {
        let none = Rejections::new();
        assert_eq!(accepted_effort("max", &none), Some("max"));
        let no_max = Rejections::from(["effort:max"]);
        assert_eq!(accepted_effort("max", &no_max), Some("xhigh"));
        let no_top = Rejections::from(["effort:max", "effort:xhigh"]);
        assert_eq!(accepted_effort("max", &no_top), Some("high"));
        assert_eq!(
            accepted_effort("minimal", &Rejections::from(["effort:minimal"])),
            Some("low")
        );
        assert_eq!(
            accepted_effort("none", &Rejections::from(["effort:none"])),
            None
        );
        assert_eq!(
            accepted_effort("medium", &Rejections::from(["effort"])),
            None
        );
    }

    #[test]
    fn a_call_streamed_early_is_not_run_twice() {
        // The same function call arrives as `response.output_item.done` AND
        // again inside the terminal response's `output`. Emitting it twice
        // would make the agent run the tool twice.
        let function_call = json!({
            "type": "function_call",
            "call_id": "call_1",
            "name": "read_file",
            "arguments": "{\"path\":\"README.md\"}",
            "status": "completed"
        });
        let mut protocol = ResponsesStream::new(Provider::Meta, "muse-spark-1.1".to_string());
        let mut events = Vec::new();
        let item_done =
            json!({"type": "response.output_item.done", "item": function_call}).to_string();
        let completed = json!({
            "type": "response.completed",
            "response": {"status": "completed", "output": [function_call]}
        })
        .to_string();
        protocol
            .on_frame(&item_done, &mut events)
            .expect("item.done");
        let flow = protocol
            .on_frame(&completed, &mut events)
            .expect("completed");
        assert_eq!(flow, Flow::Stop);
        let response = protocol.finish(&mut events).expect("finish");
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, StreamEvent::ToolCall(_)))
                .count(),
            1
        );
        assert_eq!(response.tool_calls.expect("tool calls").len(), 1);
    }

    #[test]
    fn openai_output_is_saved_under_the_model_that_wrote_it() {
        let mut protocol = ResponsesStream::new(Provider::OpenAi, "gpt-test".to_string());
        let mut events = Vec::new();
        let completed = json!({
            "type": "response.completed",
            "response": {"status": "completed", "output": [
                {"type": "compaction", "id": "cmp_1", "encrypted_content": "summary"},
                {"type": "reasoning", "id": "rs_1", "summary": []},
                {"type": "message", "role": "assistant",
                 "content": [{"type": "output_text", "text": "ok"}]}
            ]}
        })
        .to_string();
        protocol
            .on_frame(&completed, &mut events)
            .expect("completed");
        let continuation = protocol
            .finish(&mut events)
            .expect("finish")
            .provider_continuation
            .expect("continuation");
        let output = continuation.openai_output("gpt-test").expect("same model");
        assert_eq!(
            output.len(),
            2,
            "reasoning without its ciphertext is dropped"
        );
        assert!(output[0].is_compaction());
        assert!(continuation.carries_server_compaction());
        assert!(continuation.openai_output("gpt-other").is_none());
        assert!(continuation.meta_output().is_none());
    }

    /// Each `response.output_item.done` may carry a fresh `call_id`; the
    /// tool-call list is bounded by `slot_in_bounds` so a hostile stream
    /// cannot grow it (and the agent loop's work list) without limit.
    #[test]
    fn tool_calls_past_the_bound_are_ignored() {
        let mut stream = ResponsesStream::new(Provider::Meta, "muse-spark-test".to_string());
        let mut out = Vec::new();
        for i in 0..(crate::constants::MAX_TOOL_CALLS + 50) {
            let frame = format!(
                r#"{{"type":"response.output_item.done","item":{{"type":"function_call","call_id":"call_{i}","name":"read_file","arguments":"{{}}","status":"completed"}}}}"#
            );
            stream.on_frame(&frame, &mut out).unwrap();
        }
        assert_eq!(stream.tool_calls.len(), crate::constants::MAX_TOOL_CALLS);
        assert_eq!(out.len(), crate::constants::MAX_TOOL_CALLS);
    }
}
