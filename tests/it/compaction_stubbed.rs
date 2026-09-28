//! Context compaction under a scripted model, with the model call failing.
//!
//! Compaction replaces the model-visible history with a summary. It is the
//! one operation that deliberately *destroys* conversation state, so its
//! failure modes carry more blast radius than anything else the effect layer
//! does: a compaction that half-succeeds, or that reports success on nothing,
//! costs the user their session.
//!
//! It is one model call. The model writes the handoff in whatever shape it
//! judges useful; the harness does not grade it against a template or ask a
//! second call to check it. What stays enforced is the boundary:
//!
//!   * a failed call must not touch history
//!   * an empty reply must fail rather than replace real history with nothing
//!
//! The assertion in every case is `CompactionFailed` vs `CompactionFinished`,
//! because the reducer keys the history swap on exactly that distinction.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use mermaid_cli::effect::EffectRunner;
use mermaid_cli::providers::ProviderFactory;
use mermaid_cli::providers::model::ModelProvider;
use mermaid_cli::providers::tool::ToolRegistry;
use mermaid_domain::{
    ChatRequest, Cmd, CompactionPolicy, CompactionRequest, Msg, StatusKind, TurnId,
};
use mermaid_model::models::{ChatMessage, ReasoningLevel};

use crate::harness::stub_model::{ScriptedModel, Turn};

const STUB: &str = "stub/scripted";

/// A conversation big enough that a checkpoint is genuinely smaller than it.
///
/// Two separate floors have to be cleared. `prepare_compaction` needs at
/// least three messages with a non-empty head once the two-turn tail is
/// reserved — but compaction also refuses to "reduce" a history that is
/// already shorter than the checkpoint it would be replaced with, which a
/// toy fixture trips instantly.
fn history() -> Vec<ChatMessage> {
    let filler = "Discussed the precedence table, walked the token stream, and \
                  compared the output against the reference implementation. ";
    let mut messages = Vec::new();
    for i in 0..40 {
        messages.push(ChatMessage::user(format!(
            "Turn {i}: what should happen here? {}",
            filler.repeat(4)
        )));
        messages.push(ChatMessage::assistant(format!(
            "Turn {i}: here is the analysis. {}",
            filler.repeat(4)
        )));
    }
    messages
}

fn compaction_request() -> CompactionRequest {
    let chat = ChatRequest {
        model_id: STUB.to_string(),
        messages: history(),
        system_prompt: "You are a coding assistant.".to_string(),
        instructions: None,
        reasoning: ReasoningLevel::None,
        temperature: 0.7,
        max_tokens: 4096,
        tools: Vec::new(),
        ..Default::default()
    };
    CompactionRequest::manual(chat, None, CompactionPolicy::default())
}

/// The checkpoint text a compaction put into the model-visible history.
fn landed_summary(result: &mermaid_domain::CompactionResult) -> String {
    result
        .replacement_messages
        .iter()
        .map(|m| m.content.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Dispatch one compaction against `script` and return the terminal message
/// plus how many model calls it spent.
async fn compact_with(script: Vec<Turn>) -> (Msg, usize) {
    let model = ScriptedModel::new(script);
    let config = mermaid_domain::Config::default();
    let providers = Arc::new(ProviderFactory::with_seeded_providers(
        config.clone(),
        [(STUB.to_string(), model.clone() as Arc<dyn ModelProvider>)],
    ));
    let tools = Arc::new(ToolRegistry::new());
    let (mut runner, mut rx) = EffectRunner::pair_from(PathBuf::from("."), providers, tools);

    runner.dispatch(Cmd::CompactConversation {
        turn: TurnId(1),
        request: compaction_request(),
    });

    // Compaction is the only thing running, so the first Finished/Failed is
    // ours. The outer timeout turns a hang into a readable failure.
    let terminal = tokio::time::timeout(Duration::from_secs(20), async {
        while let Some(msg) = rx.recv().await {
            if matches!(
                msg,
                Msg::CompactionFinished { .. } | Msg::CompactionFailed { .. }
            ) {
                return msg;
            }
        }
        panic!("the runner closed without a compaction result");
    })
    .await
    .expect("compaction never produced a terminal message");

    runner.shutdown().await;
    (terminal, model.calls())
}

#[tokio::test]
async fn one_free_form_call_compacts() {
    // Baseline: the model answers in its own words, with no headings, and
    // that is the checkpoint. One call, no review pass.
    let handoff = "Rewriting the parser with Pratt parsing. Lexer done; precedence \
                   for unary minus still unverified. Next: add precedence tests. \
                   Marker: handoff";
    let (msg, calls) = compact_with(vec![Turn::say(handoff)]).await;
    let Msg::CompactionFinished { result, .. } = msg else {
        panic!("expected a finished compaction, got {msg:?}");
    };
    assert!(
        landed_summary(&result).contains("Marker: handoff"),
        "the model's handoff should land as written: {}",
        landed_summary(&result)
    );
    assert_eq!(calls, 1, "compaction is a single model call");
}

#[tokio::test]
async fn a_failed_call_leaves_the_conversation_alone() {
    // The provider dies on the only call. Reporting anything but a failure
    // here would have the reducer swap real history for nothing.
    let (msg, _) = compact_with(vec![Turn::fail("502 Bad Gateway")]).await;
    let Msg::CompactionFailed { message, kind, .. } = msg else {
        panic!("a dead provider must fail the compaction, got {msg:?}");
    };
    assert_eq!(
        kind,
        StatusKind::Error,
        "a provider failure is an error, not a calm note"
    );
    assert!(
        message.contains("502"),
        "the user needs the provider's reason: {message}"
    );
}

#[tokio::test]
async fn an_empty_reply_fails() {
    // Replacing the conversation with an empty checkpoint would erase it.
    let (msg, _) = compact_with(vec![Turn::say("   ")]).await;
    let Msg::CompactionFailed { message, kind, .. } = msg else {
        panic!("an empty checkpoint must fail, got {msg:?}");
    };
    assert_eq!(kind, StatusKind::Error);
    assert!(
        message.contains("empty"),
        "the failure should name what was wrong: {message}"
    );
}

#[tokio::test]
async fn a_conversation_too_short_to_compact_is_a_note_not_an_error() {
    // A benign precondition. Surfacing it as an error trains users to ignore
    // compaction errors, which is the last thing that should be ignorable.
    let model = ScriptedModel::new([]);
    let config = mermaid_domain::Config::default();
    let providers = Arc::new(ProviderFactory::with_seeded_providers(
        config,
        [(STUB.to_string(), model.clone() as Arc<dyn ModelProvider>)],
    ));
    let (mut runner, mut rx) =
        EffectRunner::pair_from(PathBuf::from("."), providers, Arc::new(ToolRegistry::new()));

    let mut request = compaction_request();
    request.chat.messages = vec![ChatMessage::user("hello")];
    runner.dispatch(Cmd::CompactConversation {
        turn: TurnId(1),
        request,
    });

    let msg = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(msg) = rx.recv().await {
            if matches!(
                msg,
                Msg::CompactionFinished { .. } | Msg::CompactionFailed { .. }
            ) {
                return msg;
            }
        }
        panic!("no compaction result");
    })
    .await
    .expect("timed out");

    let Msg::CompactionFailed { kind, .. } = msg else {
        panic!("expected the skip path, got {msg:?}");
    };
    assert_eq!(
        kind,
        StatusKind::Info,
        "nothing to compact is information, not a failure"
    );
    assert_eq!(
        model.calls(),
        0,
        "a skipped compaction must not spend a model call"
    );
    runner.shutdown().await;
}
