//! Context compaction under a scripted model, with the model calls failing.
//!
//! Compaction replaces the model-visible history with a summary. It is the
//! one operation that deliberately *destroys* conversation state, so its
//! failure modes carry more blast radius than anything else the effect layer
//! does: a compaction that half-succeeds, or that reports success on a
//! useless summary, costs the user their session.
//!
//! It is also a two-call operation — draft, then review — which no test could
//! reach before, because both calls needed a model. Everything here works by
//! making those calls fail on purpose:
//!
//!   * a draft failure must not touch history
//!   * a *review* failure must still land the valid draft, not throw the
//!     session's context away over a second call that was only ever a
//!     quality improvement
//!   * a structurally invalid summary must fail rather than replace real
//!     history with placeholder text
//!
//! The assertion in every case is `CompactionFailed` vs `CompactionFinished`,
//! because the reducer keys the history swap on exactly that distinction.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use mermaid_cli::domain::CompactionReviewStatus;
use mermaid_cli::domain::{
    ChatRequest, Cmd, CompactionPolicy, CompactionRequest, Msg, StatusKind, TurnId,
};
use mermaid_cli::effect::EffectRunner;
use mermaid_cli::models::{ChatMessage, ReasoningLevel};
use mermaid_cli::providers::ProviderFactory;
use mermaid_cli::providers::model::ModelProvider;
use mermaid_cli::providers::tool::ToolRegistry;

#[path = "harness/stub_model.rs"]
mod stub_model;
use stub_model::{ScriptedModel, Turn};

const STUB: &str = "stub/scripted";

/// A checkpoint that passes `validate_summary_structure`: all ten headings,
/// in order, each with real content.
fn valid_summary(marker: &str) -> String {
    [
        "## Goal",
        "Ship the parser rewrite.",
        "## User Preferences And Constraints",
        "Small diffs; no new dependencies.",
        "## Project State",
        &format!("Branch is green. Marker: {marker}"),
        "## Completed Work",
        "Lexer and token table.",
        "## Current Work",
        "Expression precedence.",
        "## Key Decisions",
        "Pratt parsing over recursive descent.",
        "## Critical Files And Symbols",
        "src/parse/expr.rs: parse_binary.",
        "## Commands Tests And Results",
        "cargo test parse:: passes.",
        "## Open Questions Or Risks",
        "Unary minus precedence is unverified.",
        "## Next Steps",
        "Add precedence tests.",
    ]
    .join("\n")
}

/// A conversation big enough that a checkpoint is genuinely smaller than it.
///
/// Two separate floors have to be cleared. `prepare_compaction` needs at
/// least three messages with a non-empty head once the two-turn tail is
/// reserved — but compaction also refuses to "reduce" a history that is
/// already shorter than the ten-heading checkpoint it would be replaced
/// with, which a toy fixture trips instantly.
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
fn landed_summary(result: &mermaid_cli::domain::CompactionResult) -> String {
    result
        .replacement_messages
        .iter()
        .map(|m| m.content.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Dispatch one compaction against `script` and return the terminal message.
async fn compact_with(script: Vec<Turn>) -> Msg {
    let model = ScriptedModel::new(script);
    let config = mermaid_cli::app::Config::default();
    let providers = Arc::new(ProviderFactory::with_seeded_providers(
        config.clone(),
        [(STUB.to_string(), model as Arc<dyn ModelProvider>)],
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
    terminal
}

#[tokio::test]
async fn a_good_draft_and_review_compacts() {
    // Baseline: both calls answer well, so the summary lands. Without this
    // the failure tests below could pass on a harness that never works.
    let msg = compact_with(vec![
        Turn::say(&valid_summary("draft")),
        Turn::say(&valid_summary("reviewed")),
    ])
    .await;
    let Msg::CompactionFinished { result, .. } = msg else {
        panic!("expected a finished compaction, got {msg:?}");
    };
    assert_eq!(
        result.record.review_status,
        CompactionReviewStatus::Reviewed
    );
    let landed = landed_summary(&result);
    assert!(
        landed.contains("reviewed") && !landed.contains("Marker: draft"),
        "the reviewed summary is the one that should land: {landed}"
    );
}

#[tokio::test]
async fn a_failed_draft_call_leaves_the_conversation_alone() {
    // The provider dies on the first call. Reporting anything but a failure
    // here would have the reducer swap real history for nothing.
    let msg = compact_with(vec![Turn::fail("502 Bad Gateway")]).await;
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
async fn a_failed_review_call_still_lands_the_valid_draft() {
    // The review pass is a quality improvement on a draft that already
    // validated. Throwing the whole compaction away because the second call
    // failed would cost the user their context to protect nothing.
    let msg = compact_with(vec![
        Turn::say(&valid_summary("draft")),
        Turn::fail("connection reset"),
    ])
    .await;
    let Msg::CompactionFinished { result, .. } = msg else {
        panic!("a review failure must not sink a valid draft, got {msg:?}");
    };
    assert_eq!(
        result.record.review_status,
        CompactionReviewStatus::DraftValidated,
        "the result must record that the review did not run"
    );
    assert!(
        landed_summary(&result).contains("Marker: draft"),
        "the draft should have landed: {}",
        landed_summary(&result)
    );
}

#[tokio::test]
async fn an_invalid_draft_and_invalid_review_fails() {
    // Neither call produced a structurally valid checkpoint. Replacing the
    // conversation with prose that the next turn cannot use is worse than
    // not compacting.
    let msg = compact_with(vec![
        Turn::say("Sure! Here's a summary: you were working on a parser."),
        Turn::say("Still just prose, no headings."),
    ])
    .await;
    let Msg::CompactionFailed { message, kind, .. } = msg else {
        panic!("an unusable summary must fail, got {msg:?}");
    };
    assert_eq!(kind, StatusKind::Error);
    assert!(
        message.contains("checkpoint") || message.contains("heading"),
        "the failure should name what was wrong: {message}"
    );
}

#[tokio::test]
async fn an_invalid_review_falls_back_to_the_valid_draft() {
    // Same shape as the failed review, but the model answered — badly. The
    // valid draft is still the right thing to keep.
    let msg = compact_with(vec![
        Turn::say(&valid_summary("draft")),
        Turn::say("I could not improve on that."),
    ])
    .await;
    let Msg::CompactionFinished { result, .. } = msg else {
        panic!("a malformed review must fall back to the draft, got {msg:?}");
    };
    assert_eq!(
        result.record.review_status,
        CompactionReviewStatus::DraftValidated
    );
    assert!(
        landed_summary(&result).contains("Marker: draft"),
        "{}",
        landed_summary(&result)
    );
    assert!(
        result.record.review_error.is_some(),
        "a rejected review should be recorded, not silently dropped"
    );
}

#[tokio::test]
async fn a_conversation_too_short_to_compact_is_a_note_not_an_error() {
    // A benign precondition. Surfacing it as an error trains users to ignore
    // compaction errors, which is the last thing that should be ignorable.
    let model = ScriptedModel::new([]);
    let config = mermaid_cli::app::Config::default();
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
