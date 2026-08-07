//! A scripted `ModelProvider` — a model that does exactly what a test says.
//!
//! Mermaid's agent loop is only exercised end to end when something answers
//! the model call. A live model does that faithfully but not *repeatably*:
//! it will not reliably produce a conflicting edit on demand, stop mid-write,
//! or emit a rename, and every run costs a network round trip. This stands in
//! at the provider seam, so everything below it — reducer, effect runner,
//! tool registry, safety gate, subagent spawner — is still the production
//! path.
//!
//! Injected via `ProviderFactory::with_seeded_providers`, so no production
//! dispatch knows it exists and no model id can reach it outside a test.
//!
//! # Shape
//!
//! A script is a queue of turns, consumed one per `chat` call, which is one
//! model call. `Turn::Tools` asks for tool calls; the loop runs them, feeds
//! the results back, and calls again. `Turn::Say` ends the run with a final
//! assistant message — for a subagent, that message is its report.
//!
//! ```ignore
//! let model = ScriptedModel::new([
//!     Turn::tool("write_file", json!({"path": "a.txt", "content": "hi\n"})),
//!     Turn::say("Wrote a.txt."),
//! ]);
//! ```
//!
//! Running off the end of the script is a panic, not a hang or an empty
//! reply: it means the loop took a path the test did not describe, and that
//! is a test result worth seeing.

#![allow(dead_code)] // Each test binary uses a different slice of this.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use mermaid_cli::domain::ChatRequest;
use mermaid_cli::models::{
    FinishReason, FunctionCall, ReasoningCapability, Result, TokenUsage, ToolCall,
};
use mermaid_cli::providers::capabilities::Capabilities;
use mermaid_cli::providers::model::ModelProvider;
use mermaid_cli::providers::{FinalResponse, StreamContext, StreamEvent};

/// One model call's worth of scripted output.
#[derive(Debug, Clone)]
pub enum Turn {
    /// Ask for tool calls. The loop executes them and calls back for the
    /// next turn.
    Tools(Vec<(String, serde_json::Value)>),
    /// Emit a final assistant message and stop, optionally handing back an
    /// opaque provider-continuation blob the way Meta and Anthropic do.
    Say(String, Option<mermaid_cli::models::ProviderContinuation>),
    /// Fail the call. What a provider does on a 500, a dropped connection, or
    /// an expired key — and the only way to check that a caller which is
    /// supposed to fail closed actually does.
    Fail(String),
    /// Take longer than `after` before answering. For deadlines: a caller
    /// with its own timeout can be shown to enforce it.
    Stall(std::time::Duration),
}

impl Turn {
    /// A turn requesting a single tool call.
    pub fn tool(name: &str, args: serde_json::Value) -> Self {
        Self::Tools(vec![(name.to_string(), args)])
    }

    /// A turn requesting several tool calls at once — what a real model does
    /// when it fans out, and the only way to exercise the parallel path.
    pub fn tools(calls: impl IntoIterator<Item = (String, serde_json::Value)>) -> Self {
        Self::Tools(calls.into_iter().collect())
    }

    /// A turn that ends the run with `text`.
    pub fn say(text: &str) -> Self {
        Self::Say(text.to_string(), None)
    }

    /// Hand back a continuation blob alongside this turn's text, so a test
    /// can check the loop carries it into the next request.
    pub fn with_continuation(self, c: mermaid_cli::models::ProviderContinuation) -> Self {
        match self {
            Self::Say(text, _) => Self::Say(text, Some(c)),
            other => other,
        }
    }

    /// A turn that fails with `reason`.
    pub fn fail(reason: &str) -> Self {
        Self::Fail(reason.to_string())
    }

    /// A turn that hangs for `secs` before answering.
    pub fn stall(secs: u64) -> Self {
        Self::Stall(std::time::Duration::from_secs(secs))
    }
}

/// A model that replays a fixed script.
pub struct ScriptedModel {
    name: String,
    capabilities: Capabilities,
    script: Mutex<std::collections::VecDeque<Turn>>,
    /// Every request the loop made, in order. Lets a test assert on what the
    /// agent actually asked for — the system prompt a child was given, say.
    seen: Mutex<Vec<ChatRequest>>,
}

impl ScriptedModel {
    pub fn new(script: impl IntoIterator<Item = Turn>) -> Arc<Self> {
        Arc::new(Self {
            name: "stub/scripted".to_string(),
            capabilities: Capabilities {
                supports_tools: true,
                supports_vision: false,
                supports_reasoning: ReasoningCapability::Unsupported,
                max_context_tokens: Some(1_000_000),
                max_output_tokens: Some(64_000),
                emits_provider_continuation: false,
            },
            script: Mutex::new(script.into_iter().collect()),
            seen: Mutex::new(Vec::new()),
        })
    }

    /// How many model calls the loop made. A cheap regression guard against
    /// an agent that silently starts taking twice as many turns.
    pub fn calls(&self) -> usize {
        self.seen.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// The requests the loop made, oldest first.
    pub fn requests(&self) -> Vec<ChatRequest> {
        self.seen.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Turns not yet consumed. A non-empty tail at the end of a test means
    /// the loop stopped earlier than the script expected.
    pub fn remaining(&self) -> usize {
        self.script.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

#[async_trait]
impl ModelProvider for ScriptedModel {
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    async fn chat(&self, request: ChatRequest, ctx: StreamContext) -> Result<FinalResponse> {
        self.seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(request);
        let turn = self
            .script
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front();
        let Some(turn) = turn else {
            panic!(
                "ScriptedModel ran out of script on call {}: the agent loop took a \
                 path this test did not describe",
                self.calls()
            );
        };

        let mut carried = None;
        let (tool_calls, stop_reason) = match turn {
            Turn::Fail(reason) => {
                // No `Done` — a failed call never completes its stream, which
                // is what the real adapters do on a transport error.
                return Err(mermaid_cli::models::ModelError::InvalidRequest(reason));
            },
            Turn::Stall(how_long) => {
                // Honor cancellation while stalling: a provider that ignored
                // the token would make every caller's Ctrl+C hang, and a test
                // that ignored it would hang the suite.
                tokio::select! {
                    _ = tokio::time::sleep(how_long) => {},
                    _ = ctx.token.cancelled() => {},
                }
                (Vec::new(), Some(FinishReason::Stop))
            },
            Turn::Say(text, continuation) => {
                carried = continuation;
                // Chunked, because a single-chunk stream would not exercise
                // the accumulation the real adapters rely on.
                for chunk in split_for_streaming(&text) {
                    let _ = ctx.sink.send(StreamEvent::Text(chunk)).await;
                }
                (Vec::new(), Some(FinishReason::Stop))
            },
            Turn::Tools(calls) => {
                let calls: Vec<ToolCall> = calls
                    .into_iter()
                    .enumerate()
                    .map(|(i, (name, arguments))| ToolCall {
                        id: Some(format!("stub-{}-{i}", self.calls())),
                        function: FunctionCall { name, arguments },
                    })
                    .collect();
                for call in &calls {
                    let _ = ctx.sink.send(StreamEvent::ToolCall(call.clone())).await;
                }
                (calls, Some(FinishReason::ToolUse))
            },
        };

        // The contract is exactly one `Done` at the end of a successful
        // stream; the reducer keys turn completion on it.
        let usage = Some(TokenUsage::provider(1, 1));
        let _ = ctx
            .sink
            .send(StreamEvent::Done {
                usage: usage.clone(),
                provider_continuation: carried.clone(),
                stop_reason: stop_reason.clone(),
            })
            .await;
        Ok(FinalResponse {
            usage,
            provider_continuation: carried,
            tool_calls,
            stop_reason,
        })
    }
}

/// Split text into a few chunks so the stream looks like a stream.
fn split_for_streaming(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    text.split_inclusive(' ')
        .map(str::to_string)
        .collect::<Vec<_>>()
}
