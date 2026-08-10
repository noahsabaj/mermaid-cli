//! Engine unit tests.
//!
//! Every drive outcome gets a test that reaches it, including the two that
//! only fire when something has gone wrong (the cancel grace expiring, the
//! wall-clock deadline) — a stop condition nothing has been observed to trip
//! is a stop condition nobody knows works.

use super::*;

use mermaid_domain::{Config, TurnId};
use mermaid_model::models::{FinishReason, TokenUsage};

fn fixed_now(offset_secs: i64) -> DateTime<Local> {
    DateTime::parse_from_rfc3339("2026-08-10T09:00:00+00:00")
        .expect("fixed timestamp parses")
        .with_timezone(&Local)
        + chrono::Duration::seconds(offset_secs)
}

fn fresh_state() -> State {
    State::new(
        Config::default(),
        std::path::PathBuf::from("/tmp/engine-tests"),
        "ollama/test".to_string(),
        fixed_now(0),
        std::path::PathBuf::from("/tmp"),
    )
}

/// Collects everything the reducer emitted, so a test can assert on effects
/// without an effect runtime.
#[derive(Default)]
struct Recording(Vec<Cmd>);

impl EffectSink for Recording {
    fn dispatch(&mut self, cmd: Cmd) {
        self.0.push(cmd);
    }
}

fn prompt(text: &str) -> Msg {
    Msg::SubmitPrompt {
        text: text.to_string(),
        attachment_ids: vec![],
    }
}

/// An engine mid-turn: a submitted prompt leaves the turn `Generating` and
/// parks a `CallModel` in the sink, so the drive has something to wait for.
fn generating() -> Engine<Recording, ()> {
    let mut engine = Engine::new(fresh_state(), Recording::default());
    engine.reduce(fixed_now(1), prompt("hello"));
    assert!(!engine.is_idle(), "a submitted prompt starts a turn");
    engine
}

fn channel() -> (mpsc::Sender<Msg>, mpsc::Receiver<Msg>) {
    mpsc::channel(16)
}

// ── the kernel ──────────────────────────────────────────────────────────

/// Deliberately NOT a `#[tokio::test]`: `--replay` folds a whole recorded log
/// through `reduce` with no runtime, and this is what keeps that true.
#[test]
fn reduce_stamps_the_injected_clock_and_routes_commands() {
    let mut engine = Engine::new(fresh_state(), Recording::default());

    let outcome = engine.reduce(fixed_now(7), prompt("summarize the diff"));

    assert!(!outcome.should_exit);
    assert_eq!(
        engine.state().now,
        fixed_now(7),
        "the reducer reads its clock from state, injected here"
    );
    let (_, sink, ()) = engine.into_parts();
    assert!(
        sink.0.iter().any(|c| matches!(c, Cmd::CallModel { .. })),
        "the prompt's model call must reach the sink: {:?}",
        sink.0.iter().map(Cmd::summary).collect::<Vec<_>>()
    );
}

#[test]
fn reduce_reports_the_exit_request() {
    let mut engine = Engine::new(fresh_state(), Recording::default());
    assert!(engine.reduce(fixed_now(1), Msg::Quit).should_exit);
    assert!(engine.state().should_exit);
}

/// `--replay`'s sink. The recorded log already holds every effect's real-world
/// result as a later `Msg`; re-running the effect would duplicate it.
#[test]
fn drop_effects_discards_every_command() {
    let mut engine = Engine::new(fresh_state(), DropEffects);
    engine.reduce(fixed_now(1), prompt("hello"));
    // Nothing to assert on the sink — that IS the contract. What must hold is
    // that the state still advanced.
    assert!(!engine.is_idle());
}

// ── observation ─────────────────────────────────────────────────────────

/// Records what the state looked like when each message arrived.
#[derive(Default)]
struct Watch {
    seen: Vec<(chrono::DateTime<Local>, bool)>,
}

impl StepObserver for Watch {
    async fn observe(&mut self, obs: Observation<'_>) {
        self.seen
            .push((obs.now, matches!(obs.state.turn, TurnState::Idle)));
    }
}

#[tokio::test]
async fn the_observer_sees_the_state_before_the_reducer_changes_it() {
    let mut engine =
        Engine::new(fresh_state(), Recording::default()).with_observer(Watch::default());

    engine.step_at(fixed_now(3), prompt("hello")).await;

    assert!(!engine.is_idle(), "the step left a turn in flight");
    assert_eq!(
        engine.observer.seen,
        vec![(fixed_now(3), true)],
        "the observation must be the PRE-update state (still idle), under the \
         same clock the reducer used"
    );
}

// ── drive: the ordinary outcomes ────────────────────────────────────────

#[tokio::test]
async fn a_settled_engine_returns_without_touching_the_inbox() {
    let mut engine = Engine::new(fresh_state(), Recording::default());
    let (tx, mut rx) = channel();
    tx.send(prompt("never read")).await.expect("send");

    let exit = engine
        .drive(&mut Inbox::new(&mut rx), &DrivePolicy::until_settled())
        .await;

    assert_eq!(exit, DriveExit::Settled);
    assert_eq!(rx.len(), 1, "the queued message must still be there");
}

#[tokio::test]
async fn drive_settles_once_the_turn_goes_idle() {
    let mut engine = generating();
    let turn = engine.state().turn.id().expect("turn in flight");
    let (tx, mut rx) = channel();
    tx.send(Msg::StreamText {
        turn,
        chunk: "an answer".to_string(),
    })
    .await
    .expect("send");
    tx.send(Msg::StreamDone {
        turn,
        usage: Some(TokenUsage::provider(10, 5)),
        provider_continuation: None,
        stop_reason: Some(FinishReason::Stop),
    })
    .await
    .expect("send");

    let exit = engine
        .drive(&mut Inbox::new(&mut rx), &DrivePolicy::until_settled())
        .await;

    assert_eq!(exit, DriveExit::Settled);
    assert!(engine.is_idle());
}

/// Idle is not settled while a prompt is waiting: the queue has to seed its
/// turn first. Only a cancelled run skips that (see below).
#[tokio::test(start_paused = true)]
async fn a_queued_prompt_holds_an_idle_drive_open() {
    let mut engine = Engine::new(fresh_state(), Recording::default());
    engine
        .state_mut()
        .ui
        .queued_messages
        .push_back(mermaid_domain::QueuedMessage {
            text: "still to run".to_string(),
            attachment_ids: vec![],
        });
    let (_tx, mut rx) = channel();

    // Nothing will ever arrive, so the only way out is the deadline — which is
    // the assertion: an idle engine with a queued prompt did NOT settle.
    let policy = DrivePolicy::until_settled().deadline(Some(Duration::from_secs(5)));
    let exit = engine.drive(&mut Inbox::new(&mut rx), &policy).await;

    assert_eq!(exit, DriveExit::TimedOut);
}

#[tokio::test]
async fn drive_stops_when_the_reducer_asks_to_quit() {
    let mut engine = generating();
    let (tx, mut rx) = channel();
    tx.send(Msg::Quit).await.expect("send");

    let exit = engine
        .drive(&mut Inbox::new(&mut rx), &DrivePolicy::until_exit())
        .await;

    assert_eq!(exit, DriveExit::Exited);
}

#[tokio::test]
async fn drive_reports_a_closed_message_channel() {
    let mut engine = generating();
    let (tx, mut rx) = channel();
    drop(tx);

    let exit = engine
        .drive(&mut Inbox::new(&mut rx), &DrivePolicy::until_exit())
        .await;

    assert_eq!(exit, DriveExit::Closed);
}

// ── drive: the outcomes that only fire when something is wrong ──────────

#[tokio::test(start_paused = true)]
async fn drive_stops_at_the_wall_clock_deadline() {
    let mut engine = generating();
    let (_tx, mut rx) = channel();

    let policy = DrivePolicy::until_settled().deadline(Some(Duration::from_secs(30)));
    let exit = engine.drive(&mut Inbox::new(&mut rx), &policy).await;

    assert_eq!(exit, DriveExit::TimedOut);
    assert!(
        !engine.is_idle(),
        "the caller still owns a live state after a timeout — that is why the \
         deadline is a select arm and not a timeout() wrapper (#76)"
    );
}

#[tokio::test]
async fn abort_cancellation_stops_the_drive_at_once() {
    let mut engine = generating();
    let (_tx, mut rx) = channel();
    let token = CancellationToken::new();
    token.cancel();

    let policy = DrivePolicy::until_settled().cancel_with(Some(token), OnCancel::Abort);
    let exit = engine.drive(&mut Inbox::new(&mut rx), &policy).await;

    assert_eq!(exit, DriveExit::Cancelled);
    assert!(
        matches!(engine.state().turn, TurnState::Generating { .. }),
        "Abort does not unwind the turn — the caller shuts its sink down"
    );
}

#[tokio::test(start_paused = true)]
async fn unwind_cancellation_asks_the_turn_to_end_and_waits_for_it() {
    let mut engine = generating();
    let turn = engine.state().turn.id().expect("turn in flight");
    let (tx, mut rx) = channel();
    let token = CancellationToken::new();

    // Cancel, then let the effect layer answer the way it really does: the
    // scope drops and emits the terminal `TurnCancelled`.
    let cancel = token.clone();
    tokio::spawn(async move {
        cancel.cancel();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = tx.send(Msg::TurnCancelled(turn)).await;
    });

    let policy = DrivePolicy::until_settled().cancel_with(
        Some(token),
        OnCancel::Unwind {
            grace: Duration::from_secs(15),
        },
    );
    let exit = engine.drive(&mut Inbox::new(&mut rx), &policy).await;

    assert_eq!(exit, DriveExit::Cancelled);
    assert!(engine.is_idle(), "the turn unwound inside the grace window");
}

/// The guard proven able to fail: a turn that never unwinds must not hold the
/// drive open forever. Without the grace arm this test hangs.
#[tokio::test(start_paused = true)]
async fn unwind_cancellation_hard_stops_when_the_grace_expires() {
    let mut engine = generating();
    let (_tx, mut rx) = channel();
    let token = CancellationToken::new();
    token.cancel();

    let policy = DrivePolicy::until_settled().cancel_with(
        Some(token),
        OnCancel::Unwind {
            grace: Duration::from_secs(15),
        },
    );
    let exit = engine.drive(&mut Inbox::new(&mut rx), &policy).await;

    assert_eq!(exit, DriveExit::Cancelled);
    assert!(
        matches!(engine.state().turn, TurnState::Cancelling { .. }),
        "the injected CancelTurn was reduced; the turn just never finished"
    );
}

/// The other half of `StopWhen::Settled`: normally a queued prompt holds the
/// drive open until it has had its turn, but once the run is being cancelled
/// an idle turn ends the drive whether or not the queue drained — a torn-down
/// run must not seed another turn. Drop the `unwind_by` clause from the settle
/// check and this hangs until the grace expires.
#[tokio::test(start_paused = true)]
async fn a_cancelled_drive_stops_with_prompts_still_queued() {
    let mut engine = Engine::new(fresh_state(), Recording::default());
    engine
        .state_mut()
        .ui
        .queued_messages
        .push_back(mermaid_domain::QueuedMessage {
            text: "queued behind the turn".to_string(),
            attachment_ids: vec![],
        });
    let (_tx, mut rx) = channel();
    let token = CancellationToken::new();
    token.cancel();

    let policy = DrivePolicy::until_settled().cancel_with(
        Some(token),
        OnCancel::Unwind {
            grace: Duration::from_secs(15),
        },
    );
    let exit = engine.drive(&mut Inbox::new(&mut rx), &policy).await;

    assert_eq!(exit, DriveExit::Cancelled);
    assert!(
        !engine.state().ui.queued_messages.is_empty(),
        "the queued prompt is still queued, not started"
    );
}

// ── the inbox ───────────────────────────────────────────────────────────

#[tokio::test]
async fn the_inbox_merges_lifecycle_signals() {
    let mut engine = generating();
    let (_tx, mut rx) = channel();
    let (signals, mut lifecycle) = RuntimeLifecycle::for_test();
    signals
        .send(mermaid_domain::RuntimeSignal::Terminate)
        .expect("send signal");

    let exit = engine
        .drive(
            &mut Inbox::new(&mut rx).with_lifecycle(&mut lifecycle),
            &DrivePolicy::until_exit(),
        )
        .await;

    assert_eq!(
        exit,
        DriveExit::Exited,
        "SIGTERM unwinds through the reducer"
    );
    assert!(engine.state().should_exit);
}

/// A dead signal channel drops out of the select instead of being polled
/// forever. `UnboundedReceiver::recv` on a closed channel is instantly ready,
/// so re-selecting on it would spin the drive hot.
#[tokio::test]
async fn a_closed_lifecycle_channel_does_not_spin_the_drive() {
    let mut engine = generating();
    let (tx, mut rx) = channel();
    let (signals, mut lifecycle) = RuntimeLifecycle::for_test();
    drop(signals);
    tx.send(Msg::Quit).await.expect("send");

    let exit = engine
        .drive(
            &mut Inbox::new(&mut rx).with_lifecycle(&mut lifecycle),
            &DrivePolicy::until_exit(),
        )
        .await;

    assert_eq!(exit, DriveExit::Exited);
}

/// `TurnId` is only reachable here through the state, so this keeps the import
/// honest if the helper above ever stops needing it.
#[test]
fn a_fresh_state_has_no_turn() {
    assert_eq!(fresh_state().turn.id(), None::<TurnId>);
}
