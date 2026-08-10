//! Talking to a run that is already happening.
//!
//! `EngineHandle` claims a running session has a mailbox: something outside the
//! process's own effects can put a `Msg` in, and it is handled exactly like one
//! the run produced itself. The engine's unit tests make that claim against a
//! reducer with no provider behind it; this makes it against a real drive —
//! real effect runner, real model calls (scripted), real turn transitions —
//! because "the prompt queues" and "the prompt gets answered" are different
//! claims, and only the second one is what a caller wants.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use mermaid_cli::effect::EffectRunner;
use mermaid_cli::engine::{DriveExit, DrivePolicy, Engine, EngineHandle, Inbox};
use mermaid_cli::providers::ProviderFactory;
use mermaid_cli::providers::model::ModelProvider;
use mermaid_cli::providers::tool::ToolRegistry;
use mermaid_domain::{Config, Msg, State};
use mermaid_model::models::MessageRole;

#[path = "harness/stub_model.rs"]
mod stub_model;
use stub_model::{ScriptedModel, Turn};

const STUB: &str = "stub/scripted";

fn engine_with(
    model: Arc<ScriptedModel>,
) -> (
    Engine<EffectRunner>,
    tokio::sync::mpsc::Receiver<Msg>,
    EngineHandle<()>,
) {
    let providers = Arc::new(ProviderFactory::with_seeded_providers(
        Config::default(),
        [(STUB.to_string(), model as Arc<dyn ModelProvider>)],
    ));
    let (runner, rx) =
        EffectRunner::pair_from(PathBuf::from("."), providers, Arc::new(ToolRegistry::new()));
    let handle = EngineHandle::with_capacity(runner.sender(), 8);
    let state = State::new(
        Config::default(),
        PathBuf::from("."),
        STUB.to_string(),
        chrono::Local::now(),
        std::env::temp_dir(),
    );
    (Engine::new(state, runner), rx, handle)
}

fn prompt(text: &str) -> Msg {
    Msg::SubmitPrompt {
        text: text.to_string(),
        attachment_ids: vec![],
    }
}

fn said(state: &State, role: &MessageRole) -> Vec<String> {
    state
        .session
        .messages()
        .iter()
        .filter(|m| m.role == *role)
        .map(|m| m.content.clone())
        .collect()
}

/// The whole point, end to end: a prompt that nobody typed arrives while a turn
/// is in flight, waits its turn, and gets its own answer from the model.
#[tokio::test]
async fn a_prompt_sent_through_the_handle_gets_answered() {
    let model = ScriptedModel::new([Turn::say("first answer"), Turn::say("second answer")]);
    let (mut engine, mut rx, handle) = engine_with(model.clone());

    // Sent before the drive starts pumping, which is what makes the test
    // deterministic: the follow-up is at the head of the inbox, ahead of
    // anything the model will say. It is still delivered INTO a live turn —
    // the seed below starts one, and the drive reduces the follow-up while
    // that turn is generating, which is why it queues instead of starting.
    handle
        .send(prompt("and the second thing"))
        .await
        .expect("engine is running");

    engine.reduce(chrono::Local::now(), prompt("the first thing"));
    assert!(!engine.is_idle(), "the seed started a turn");

    let exit = engine
        .drive(
            &mut Inbox::new(&mut rx),
            &DrivePolicy::until_settled().deadline(Some(Duration::from_secs(20))),
        )
        .await;

    assert_eq!(exit, DriveExit::Settled);
    assert_eq!(
        said(engine.state(), &MessageRole::User),
        vec!["the first thing", "and the second thing"],
        "the sent prompt joined the transcript as a user message"
    );
    assert_eq!(
        said(engine.state(), &MessageRole::Assistant),
        vec!["first answer", "second answer"],
        "and the model answered it in a turn of its own"
    );
    assert_eq!(model.calls(), 2, "two prompts, two model calls");
}

/// A handle outlives nothing: once the drive is done, the mailbox is shut and
/// says so, handing the message back rather than swallowing it.
#[tokio::test]
async fn the_handle_closes_when_the_run_is_over() {
    let model = ScriptedModel::new([Turn::say("done")]);
    let (mut engine, mut rx, handle) = engine_with(model);

    engine.reduce(chrono::Local::now(), prompt("anything"));
    let exit = engine
        .drive(
            &mut Inbox::new(&mut rx),
            &DrivePolicy::until_settled().deadline(Some(Duration::from_secs(20))),
        )
        .await;
    assert_eq!(exit, DriveExit::Settled);

    // The run is over: its state and runner go back to the caller, and the
    // inbox receiver goes with them.
    let (_state, runner, ()) = engine.into_parts();
    runner.shutdown().await;
    drop(rx);

    assert!(!handle.is_running());
    let returned = handle
        .send(prompt("too late"))
        .await
        .expect_err("a finished run takes no messages");
    assert!(matches!(returned.message(), Msg::SubmitPrompt { text, .. } if text == "too late"));
}
