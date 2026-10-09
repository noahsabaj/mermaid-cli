//! Meta Model API adapter, on the Responses endpoint.
//!
//! Unlike Meta's OpenAI-compatible Chat Completions surface, Responses can
//! carry encrypted reasoning across tool turns. The wire format, stream and
//! replay are shared with OpenAI's adapter (see `responses`); this file holds
//! what is Meta's own: the muse-spark limits, effort tiers and temperature.
//!
//! The fifth adapter, and the last to arrive. It spent its first life as a
//! `ModelProvider` in the CLI crate, hand-rolling everything a `Model`
//! adapter already had: its own SSE loop, its own reassembly cap, its own
//! status-error handler, its own cancellation check. What kept it there was
//! one dependency — it built its request straight from `ChatRequest`, which
//! lives in `mermaid-domain`, one crate ABOVE this one. Taking
//! `&[ChatMessage]` and a `ModelConfig` like its four siblings is the whole
//! of what moving it required.

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde_json::{Value, json};

use super::accumulator::http_error;
use super::learning::{Learning, Optional, ParamMemory, Rejections};
use super::responses::{
    Provider, Replay, ResponsesStream, accepted_effort, combined_instructions, function_tools,
    messages_to_input, sent_effort,
};
use crate::models::adapters::driver::drive_stream;
use crate::models::capabilities::ModelCapabilities;
use crate::models::config::ModelConfig;
use crate::models::error::{BackendError, ModelError, Result};
use crate::models::reasoning::{ReasoningCapability, ReasoningLevel, nearest_effort};
use crate::models::stream::StreamSink;
use crate::models::traits::Model;
use crate::models::types::{ChatMessage, ModelResponse, ProviderContinuation};

/// Meta's Responses-API root, and the env var its key lives in.
pub const DEFAULT_BASE_URL: &str = "https://api.meta.ai/v1";
pub const DEFAULT_API_KEY_ENV: &str = "MODEL_API_KEY";

pub struct MetaAdapter {
    client: Client,
    base_url: String,
    api_key: String,
    model_name: String,
    extra_headers: HashMap<String, String>,
    capabilities: ModelCapabilities,
    /// What this model's provider rejected (see `learning`).
    memory: ParamMemory,
}

impl MetaAdapter {
    /// Build the Responses-API adapter for a Meta model.
    ///
    /// # Errors
    ///
    /// Only the HTTP client build, as [`BackendError::ConnectionFailed`]. The
    /// API is not contacted here; a `model_name` outside the `muse-spark`
    /// family is not an error either — it simply advertises no documented
    /// context or output limits.
    pub fn new(
        api_key: String,
        model_name: String,
        base_url: String,
        extra_headers: HashMap<String, String>,
    ) -> Result<Self> {
        let client = Client::builder()
            .pool_max_idle_per_host(10)
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(60))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|error| {
                ModelError::Backend(BackendError::ConnectionFailed {
                    backend: "meta".to_string(),
                    url: base_url.clone(),
                    reason: error.to_string(),
                })
            })?;
        // Prefix, not exact-id: a future muse-spark-1.4 should inherit the
        // documented family limits instead of regressing to "unknown".
        let muse_spark = model_name.to_ascii_lowercase().starts_with("muse-spark");
        // The one sanctioned static-window exception (see capabilities.rs's
        // module doc): Meta documents the muse-spark family limits and
        // exposes no endpoint to discover them live.
        let capabilities = ModelCapabilities {
            max_context_tokens: muse_spark
                .then_some(crate::constants::META_MUSE_SPARK_CONTEXT_WINDOW),
            max_output_tokens: muse_spark
                .then_some(crate::constants::META_MUSE_SPARK_MAX_OUTPUT_TOKENS),
            ..ModelCapabilities::advertised(
                true,
                ReasoningCapability::Levels(meta_reasoning_levels(&model_name)),
            )
            .with_provider_continuation()
        };
        Ok(Self {
            client,
            base_url,
            api_key,
            model_name,
            extra_headers,
            capabilities,
            memory: ParamMemory::default(),
        })
    }

    /// What this model's provider has rejected, for the wrapper to persist
    /// and to seed from the cache.
    #[must_use]
    pub const fn param_memory(&self) -> &ParamMemory {
        &self.memory
    }

    async fn send_chat(&self, body: &Value) -> Result<reqwest::Response> {
        let url = format!("{}/responses", self.base_url.trim_end_matches('/'));
        let mut builder = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .header("Accept", "text/event-stream")
            .json(body);
        for (name, value) in &self.extra_headers {
            builder = builder.header(name, value);
        }
        builder.send().await.map_err(|error| {
            ModelError::Backend(BackendError::ConnectionFailed {
                backend: "meta".to_string(),
                url,
                reason: error.to_string(),
            })
        })
    }
}

#[async_trait]
impl Model for MetaAdapter {
    fn name(&self) -> &str {
        &self.model_name
    }

    fn capabilities(&self) -> &ModelCapabilities {
        &self.capabilities
    }

    async fn chat(
        &self,
        messages: &[ChatMessage],
        config: &ModelConfig,
        sink: Option<StreamSink>,
    ) -> Result<ModelResponse> {
        // Responses is a streaming-only surface here: mermaid always asks for
        // `stream: true` because that is the only shape the encrypted
        // reasoning items arrive in. A sink-less call still drives the same
        // stream, it just drops the events.
        // Optimistic send; a 400 naming an optional parameter takes it back
        // and retries (see `learning`).
        let mut learning = Learning::start(&self.memory, &self.model_name, sink.as_ref());
        let response = loop {
            let body =
                build_request_body_with(messages, config, &self.model_name, learning.rejections());
            let response = self.send_chat(&body).await?;
            if !learning.is_retryable(&response) {
                break response;
            }
            let err = meta_http_error(response).await;
            learning.retry_or_fail(err, &sent_optionals(&body)).await?;
        };
        learning.settle(&response);
        if !response.status().is_success() {
            return Err(meta_http_error(response).await);
        }
        drive_stream(
            response.bytes_stream(),
            ResponsesStream::new(Provider::Meta, self.model_name.clone()),
            sink.as_ref(),
        )
        .await
    }
}

#[cfg(test)]
pub(crate) fn build_request_body(
    messages: &[ChatMessage],
    config: &ModelConfig,
    model_name: &str,
) -> Value {
    build_request_body_with(messages, config, model_name, &Rejections::new())
}

/// The Responses-API request body, avoiding what the provider already
/// rejected for this model.
pub(crate) fn build_request_body_with(
    messages: &[ChatMessage],
    config: &ModelConfig,
    model_name: &str,
    rejected: &Rejections,
) -> Value {
    let effort = nearest_effort(config.reasoning, &meta_reasoning_levels(model_name))
        .unwrap_or(ReasoningLevel::Minimal);
    let mut reasoning = json!({"summary": "auto"});
    if let Some(effort) = accepted_effort(meta_effort(effort), rejected) {
        reasoning["effort"] = json!(effort);
    }
    let mut body = json!({
        "model": model_name,
        "input": messages_to_input(
            messages,
            ProviderContinuation::meta_output,
            Replay { strip_ids: false },
        ),
        "stream": true,
        "store": false,
        "include": ["reasoning.encrypted_content"],
        "reasoning": reasoning,
    });
    // Muse is tuned for Meta's 1.0 default. Mermaid's global 0.7 default was
    // chosen for other providers, so omit it here unless the user changed it.
    if (config.temperature - crate::constants::DEFAULT_TEMPERATURE).abs() > f32::EPSILON
        && !rejected.contains("temperature")
    {
        body["temperature"] = json!(config.temperature);
    }
    let instructions = combined_instructions(config);
    if !instructions.is_empty() {
        body["instructions"] = Value::String(instructions);
    }
    let tools = function_tools(&config.tools, |_| false);
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    if config.max_tokens > 0 {
        let limit = config
            .resolved_max_output
            .map_or(config.max_tokens, |max| config.max_tokens.min(max));
        body["max_output_tokens"] = json!(limit);
    }
    body
}

/// The optional items a built request carries, for blaming a rejection.
/// Read back off the body so it can't drift from what was actually sent.
fn sent_optionals(body: &Value) -> Vec<Optional> {
    let mut sent = Vec::new();
    if body.get("temperature").is_some() {
        sent.push(Optional::new(
            "temperature",
            "temperature",
            &["temperature"],
        ));
    }
    sent.extend(sent_effort(body));
    sent
}

/// Whether this model id supports the `max` reasoning tier.
///
/// `max` went public on `muse-spark-1.3` (Sept 4, after the launch-gating
/// safety review); 1.1/1.2 top out at `xhigh`, so a `Max` request for them
/// keeps snapping down via `nearest_effort`. The minor is parsed rather
/// than matched so a future 1.4 inherits `max`; an id with no parseable
/// `1.N` version is assumed current (same forward-compat rule as the
/// context-window prefix above).
fn meta_supports_max(model_name: &str) -> bool {
    let lower = model_name.to_ascii_lowercase();
    let Some((_, rest)) = lower.split_once("muse-spark-") else {
        return true;
    };
    // Expect `1.N...`, possibly with a `-suffix` like `-contributor` on the
    // minor. Major 2+, or an unrecognized shape, assumes current.
    let mut parts = rest.split('.');
    match (parts.next(), parts.next()) {
        (Some("1"), Some(minor)) => {
            let digits: String = minor.chars().take_while(char::is_ascii_digit).collect();
            digits.parse::<u64>().map_or(true, |n| n >= 3)
        },
        _ => true,
    }
}

fn meta_reasoning_levels(model_name: &str) -> Vec<ReasoningLevel> {
    let mut levels = vec![
        ReasoningLevel::Minimal,
        ReasoningLevel::Low,
        ReasoningLevel::Medium,
        ReasoningLevel::High,
        ReasoningLevel::XHigh,
    ];
    if meta_supports_max(model_name) {
        levels.push(ReasoningLevel::Max);
    }
    levels
}

fn meta_effort(level: ReasoningLevel) -> &'static str {
    match level {
        ReasoningLevel::None | ReasoningLevel::Minimal => "minimal",
        ReasoningLevel::Low => "low",
        ReasoningLevel::Medium => "medium",
        ReasoningLevel::High => "high",
        ReasoningLevel::XHigh => "xhigh",
        // Reachable only when the model id advertises `Max`; older models
        // snap down to `XHigh` in `nearest_effort` before reaching here.
        ReasoningLevel::Max => "max",
    }
}

/// Meta's own status-error handler, kept rather than shared: it redacts the
/// body, and a Responses 4xx routinely echoes the `Authorization` header
/// back inside the message.
async fn meta_http_error(response: reqwest::Response) -> ModelError {
    http_error(response, "Meta request failed").await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::types::ChatMessageKind;

    fn config() -> ModelConfig {
        ModelConfig {
            model: "meta/muse-spark-1.1".to_string(),
            temperature: crate::constants::DEFAULT_TEMPERATURE,
            max_tokens: 200_000,
            reasoning: ReasoningLevel::Max,
            system_prompt: Some("system".to_string()),
            dynamic_system_suffix: Some("project".to_string()),
            tools: vec![json!({
                "type": "function",
                "function": {
                    "name": "read_file",
                    "description": "Read a file",
                    "parameters": {"type": "object"},
                }
            })],
            resolved_max_output: Some(crate::constants::META_MUSE_SPARK_MAX_OUTPUT_TOKENS),
            ..Default::default()
        }
    }

    fn messages() -> Vec<ChatMessage> {
        vec![ChatMessage::user("hello").with_images(vec!["PNG".to_string()])]
    }

    #[test]
    fn request_uses_stateless_encrypted_replay_shape() {
        let body = build_request_body(&messages(), &config(), "muse-spark-1.1");
        assert_eq!(body["store"], false);
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(body["reasoning"]["effort"], "xhigh");
        assert_eq!(body["reasoning"]["summary"], "auto");
        assert_eq!(
            body["max_output_tokens"],
            crate::constants::META_MUSE_SPARK_MAX_OUTPUT_TOKENS
        );
        assert_eq!(body["instructions"], "system\n\nproject");
        assert_eq!(body["tools"][0]["name"], "read_file");
        assert!(body.get("temperature").is_none());
        assert!(body.get("previous_response_id").is_none());
        assert!(body.get("tool_choice").is_none());
        assert_eq!(body["input"][0]["content"][1]["type"], "input_image");
    }

    #[test]
    fn tool_definitions_lose_the_openai_function_wrapper() {
        // Responses takes `name`/`parameters` flat; the config carries them
        // in the Chat Completions envelope every other adapter reads.
        let body = build_request_body(&messages(), &config(), "muse-spark-1.1");
        let tool = &body["tools"][0];
        assert_eq!(tool["type"], "function");
        assert_eq!(tool["name"], "read_file");
        assert_eq!(tool["description"], "Read a file");
        assert_eq!(tool["parameters"], json!({"type": "object"}));
        assert!(tool.get("function").is_none());
    }

    /// Adapter contract (see `MessageAudience`): harness steering must reach
    /// the model. The Responses API carries system-role input messages, so the
    /// reminder passes through in place at the tail.
    #[test]
    fn model_directed_system_messages_reach_the_wire_in_place() {
        let mut msgs = messages();
        let mut nudge = ChatMessage::system("Reminder: the task checklist is stale.");
        nudge.kind = ChatMessageKind::RecoveryNudge;
        msgs.push(nudge);
        let body = build_request_body(&msgs, &config(), "muse-spark-1.1");

        let input = body["input"].as_array().expect("input array");
        let last = input.last().expect("non-empty");
        assert_eq!(last["role"], "system");
        assert!(
            serde_json::to_string(&last["content"])
                .unwrap()
                .contains("the task checklist is stale"),
        );
    }

    #[test]
    fn none_reasoning_maps_to_minimal_and_auto_budget_is_omitted() {
        let cfg = ModelConfig {
            reasoning: ReasoningLevel::None,
            max_tokens: 0,
            ..config()
        };
        let body = build_request_body(&messages(), &cfg, "muse-spark-1.1");
        assert_eq!(body["reasoning"]["effort"], "minimal");
        assert!(body.get("max_output_tokens").is_none());
    }

    #[test]
    fn max_reasoning_sends_max_on_1_3() {
        // `max` went public on muse-spark-1.3; the request must carry it
        // verbatim, including the `-contributor` suffixed id.
        for model in [
            "muse-spark-1.3",
            "muse-spark-1.3-contributor",
            "MUSE-SPARK-1.3",
        ] {
            let body = build_request_body(&messages(), &config(), model);
            assert_eq!(body["reasoning"]["effort"], "max", "model {model}");
        }
        // XHigh stays xhigh on 1.3 — the tier below max must not over-deliver.
        let mut cfg = config();
        cfg.reasoning = ReasoningLevel::XHigh;
        let body = build_request_body(&messages(), &cfg, "muse-spark-1.3");
        assert_eq!(body["reasoning"]["effort"], "xhigh");
    }

    #[test]
    fn max_reasoning_snaps_to_xhigh_before_1_3() {
        // 1.1/1.2 top out at xhigh: a Max request downgrades rather than
        // sending a value the API rejects.
        for model in ["muse-spark-1.1", "muse-spark-1.2"] {
            let body = build_request_body(&messages(), &config(), model);
            assert_eq!(body["reasoning"]["effort"], "xhigh", "model {model}");
        }
    }

    #[test]
    fn a_rejected_effort_drops_the_field_and_keeps_the_summary() {
        let body = build_request_body_with(
            &messages(),
            &config(),
            "muse-spark-1.1",
            &Rejections::from(["effort"]),
        );
        assert!(body["reasoning"].get("effort").is_none());
        assert_eq!(body["reasoning"]["summary"], "auto");
    }

    #[test]
    fn meta_supports_max_gates_on_minor_version() {
        for model in [
            "muse-spark-1.3",
            "muse-spark-1.3-contributor",
            "muse-spark-1.4",
            "muse-spark-2.0",
            "muse-spark",
            "something-else-entirely",
        ] {
            assert!(meta_supports_max(model), "model {model} should support max");
        }
        for model in ["muse-spark-1.1", "muse-spark-1.2", "MUSE-SPARK-1.1"] {
            assert!(
                !meta_supports_max(model),
                "model {model} should not support max"
            );
        }
    }
}
