//! The main agent turn, driven by a scripted model.
//!
//! Most of the loop's *logic* is already covered without a stub, because the
//! reducer is pure: stale-turn drops, cancel transitions, and the whole
//! truncation-recovery ladder are unit-tested in `domain::reducer` and
//! `tests/reducer_flows.rs`, and tool cancellation has `tests/effect_cancel.rs`.
//! Those are not repeated here.
//!
//! What is left over is the part that only exists once a *provider* is in the
//! loop — where the effect layer sits between the model and the reducer:
//!
//!   * **Provider continuation round-trips.** Meta's encrypted reasoning and
//!     Anthropic's thinking signature are opaque blobs the provider hands back
//!     and the next request must carry. Break that and nothing errors: the
//!     model silently loses its reasoning across every tool call, which reads
//!     as the model getting dumber rather than as a bug. `types.rs` covers the
//!     serde round trip of the blob; this covers the loop actually carrying it.
//!   * **Parallel tool calls.** One model turn emitting three calls must run
//!     three tools and feed all three results back.
//!   * **Provider errors reach the user intact**, with the provider's own
//!     reason rather than a flattened "something went wrong".

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use mermaid_cli::effect::EffectRunner;
use mermaid_cli::providers::ProviderFactory;
use mermaid_cli::providers::model::ModelProvider;
use mermaid_cli::providers::tool::ToolRegistry;
use mermaid_domain::{ChatRequest, Cmd, Msg, TurnId};
use mermaid_model::models::{ChatMessage, ProviderContinuation, ReasoningLevel};

use crate::harness::stub_model::{ScriptedModel, Turn};

const STUB: &str = "stub/scripted";

fn request(messages: Vec<ChatMessage>) -> ChatRequest {
    ChatRequest {
        model_id: STUB.to_string(),
        messages,
        system_prompt: "You are a coding assistant.".to_string(),
        instructions: None,
        reasoning: ReasoningLevel::None,
        temperature: 0.7,
        max_tokens: 4096,
        tools: Vec::new(),
        ..Default::default()
    }
}

fn runner_with(model: Arc<ScriptedModel>) -> (EffectRunner, tokio::sync::mpsc::Receiver<Msg>) {
    let providers = Arc::new(ProviderFactory::with_seeded_providers(
        mermaid_domain::Config::default(),
        [(STUB.to_string(), model as Arc<dyn ModelProvider>)],
    ));
    EffectRunner::pair_from(PathBuf::from("."), providers, Arc::new(ToolRegistry::new()))
}

/// Drain messages until `f` matches one, or time out with a readable failure.
async fn wait_for<T>(
    rx: &mut tokio::sync::mpsc::Receiver<Msg>,
    what: &str,
    mut f: impl FnMut(&Msg) -> Option<T>,
) -> T {
    tokio::time::timeout(Duration::from_secs(20), async {
        while let Some(msg) = rx.recv().await {
            if let Some(hit) = f(&msg) {
                return hit;
            }
        }
        panic!("the runner closed before {what}");
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

#[tokio::test]
async fn a_provider_continuation_rides_the_next_request() {
    // The blob the provider hands back on turn one must be attached to the
    // assistant message, so turn two carries it upstream. Nothing errors when
    // this breaks — the model just quietly loses its reasoning.
    let model = ScriptedModel::new([Turn::say("Thinking about it.").with_continuation(
        ProviderContinuation::Anthropic {
            signature: "opaque-thinking-signature".to_string(),
        },
    )]);
    let (mut runner, mut rx) = runner_with(model.clone());

    runner.dispatch(Cmd::CallModel {
        turn: TurnId(1),
        request: request(vec![ChatMessage::user("hello")]),
    });

    let continuation = wait_for(&mut rx, "the turn to finish", |msg| match msg {
        Msg::StreamDone {
            provider_continuation,
            ..
        } => Some(provider_continuation.clone()),
        _ => None,
    })
    .await;
    assert_eq!(
        continuation,
        Some(ProviderContinuation::Anthropic {
            signature: "opaque-thinking-signature".to_string()
        }),
        "the effect layer must surface the provider's continuation to the reducer"
    );
    runner.shutdown().await;
}

#[tokio::test]
async fn a_continuation_attached_to_history_is_sent_back_upstream() {
    // The other half of the round trip: an assistant message carrying a blob
    // must still be carrying it when the next request goes out.
    let model = ScriptedModel::new([Turn::say("Continuing.")]);
    let (mut runner, mut rx) = runner_with(model.clone());

    let prior = ChatMessage::assistant("Step one done.").with_provider_continuation(
        ProviderContinuation::Anthropic {
            signature: "carried-signature".to_string(),
        },
    );
    runner.dispatch(Cmd::CallModel {
        turn: TurnId(1),
        request: request(vec![
            ChatMessage::user("go"),
            prior,
            ChatMessage::user("next"),
        ]),
    });

    wait_for(&mut rx, "the turn to finish", |msg| {
        matches!(msg, Msg::StreamDone { .. }).then_some(())
    })
    .await;

    let sent = model.requests();
    let carried = sent[0]
        .messages
        .iter()
        .filter_map(|m| m.provider_continuation.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        carried,
        vec![ProviderContinuation::Anthropic {
            signature: "carried-signature".to_string()
        }],
        "the request that left the process dropped the continuation"
    );
    runner.shutdown().await;
}

#[tokio::test]
async fn one_turn_emitting_several_tool_calls_reports_them_all() {
    // Fan-out inside a single turn. The reducer gates the follow-up call on
    // every outcome landing, so losing one here would hang the turn.
    let model = ScriptedModel::new([Turn::tools([
        (
            "read_file".to_string(),
            serde_json::json!({"path": "a.txt"}),
        ),
        (
            "read_file".to_string(),
            serde_json::json!({"path": "b.txt"}),
        ),
        (
            "read_file".to_string(),
            serde_json::json!({"path": "c.txt"}),
        ),
    ])]);
    let (mut runner, mut rx) = runner_with(model.clone());

    runner.dispatch(Cmd::CallModel {
        turn: TurnId(1),
        request: request(vec![ChatMessage::user("read the files")]),
    });

    // Tool calls arrive as their own messages before `StreamDone`, so
    // collect them until the turn ends.
    let mut calls = Vec::new();
    wait_for(&mut rx, "the turn to finish", |msg| match msg {
        Msg::StreamToolCall { call, .. } => {
            calls.push(call.clone());
            None
        },
        Msg::StreamDone { .. } => Some(()),
        _ => None,
    })
    .await;
    assert_eq!(calls.len(), 3, "all three calls must reach the reducer");
    let paths: Vec<String> = calls
        .iter()
        .filter_map(|c| {
            c.function
                .arguments
                .get("path")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .collect();
    assert_eq!(
        paths,
        vec!["a.txt", "b.txt", "c.txt"],
        "arguments must survive"
    );
    runner.shutdown().await;
}

#[tokio::test]
async fn cancelling_a_turn_aborts_an_in_flight_model_call() {
    // `tests/effect_cancel.rs` covers cancelling a running *tool*. This is the
    // other half: Ctrl+C while the model itself is still streaming. The
    // provider contract requires selecting on the turn token, and a provider
    // that ignored it would leave the user's Ctrl+C hanging on a long
    // generation with nothing to interrupt it.
    let model = ScriptedModel::new([Turn::stall(60)]);
    let (mut runner, mut rx) = runner_with(model);

    runner.dispatch(Cmd::CallModel {
        turn: TurnId(1),
        request: request(vec![ChatMessage::user("write me an essay")]),
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let cancelled_at = std::time::Instant::now();
    runner.dispatch(Cmd::CancelScope(TurnId(1)));

    // Any terminal message for the turn will do; what matters is that one
    // arrives long before the 60-second stall would have ended.
    wait_for(&mut rx, "the cancelled turn to unwind", |msg| {
        matches!(
            msg,
            Msg::StreamDone { .. } | Msg::UpstreamError { .. } | Msg::TurnCancelled { .. }
        )
        .then_some(())
    })
    .await;
    assert!(
        cancelled_at.elapsed() < Duration::from_secs(10),
        "cancellation took {:?} — the model call did not honor the turn token",
        cancelled_at.elapsed()
    );
    runner.shutdown().await;
}

#[tokio::test]
async fn a_provider_error_reaches_the_user_with_its_reason() {
    // Flattening this to a generic failure is how a fixable problem (an
    // expired key, a rate limit) becomes an unactionable one.
    let model = ScriptedModel::new([Turn::fail("rate limited: retry after 30s")]);
    let (mut runner, mut rx) = runner_with(model);

    runner.dispatch(Cmd::CallModel {
        turn: TurnId(1),
        request: request(vec![ChatMessage::user("hello")]),
    });

    let text = wait_for(&mut rx, "an upstream error", |msg| match msg {
        Msg::UpstreamError { error, .. } => Some(format!("{error:?}")),
        _ => None,
    })
    .await;
    assert!(
        text.contains("rate limited") && text.contains("30s"),
        "the provider's own reason must survive to the user: {text}"
    );
    runner.shutdown().await;
}
