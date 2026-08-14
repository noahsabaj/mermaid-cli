//! The driving loop, as a value.
//!
//! `update(State, Msg) -> (State, Vec<Cmd>)` is the product; driving it is five
//! lines that were written out longhand in six places, each with its own
//! spelling of the surrounding loop. [`Engine`] owns the reducer state and the
//! effect sink and exposes those five lines once:
//!
//! ```text
//!   inbox ── Msg ──► observer ──► update(State, Msg) ──► (State, Vec<Cmd>) ──► sink
//! ```
//!
//! Three seams, one per axis the callers actually differ on: [`EffectSink`]
//! (where a `Cmd` goes), [`StepObserver`] (what watches each message before the
//! reducer consumes it), and [`DrivePolicy`] (when the loop stops).
//!
//! [`Engine::drive`] is an actor loop, and [`EngineHandle`] names its two ends
//! so something outside it — a daemon socket, a second view of one session, an
//! SDK client — can send messages in and watch what comes out.
//!
//! [`Engine::reduce`] — the kernel — is synchronous and observer-free on
//! purpose: `--replay` folds a recorded log with no tokio runtime in sight, and
//! keeping the kernel callable from a plain `for` loop is what proves this
//! abstraction did not smuggle a runtime into the fold.
//!
//! See `docs/design/engine-extraction.md`.

mod handle;

pub use handle::{EngineGone, EngineHandle};

use std::time::Duration;

use chrono::{DateTime, Local};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::app::lifecycle::RuntimeLifecycle;
use crate::effect::EffectRunner;
use mermaid_domain::{Cmd, Msg, State, TurnState, update};

/// Where a reducer-emitted `Cmd` goes.
///
/// The live sink is [`EffectRunner`]; `--replay` uses [`DropEffects`]. It is
/// also the interception point for commands a specific driver owns rather than
/// the effect layer — the interactive loop's `Cmd::ComposeInEditor` suspends
/// the terminal and the crossterm event stream, which only that loop holds.
pub trait EffectSink {
    fn dispatch(&mut self, cmd: Cmd);
}

impl EffectSink for EffectRunner {
    fn dispatch(&mut self, cmd: Cmd) {
        // The inherent method; this impl only exposes it through the seam.
        Self::dispatch(self, cmd);
    }
}

/// Discards every command. `--replay`'s sink: the recorded log already holds
/// each effect's real-world result as a later `Msg`, so re-running the effect
/// would both duplicate it and make the fold impure.
#[derive(Debug, Default, Clone, Copy)]
pub struct DropEffects;

impl EffectSink for DropEffects {
    fn dispatch(&mut self, _cmd: Cmd) {}
}

/// One message, as seen *before* `update` consumes it.
///
/// Pre-update is the whole point: the recorder logs the input that produced a
/// state, the `RunEvent` projection reads fields the reducer strips, and the
/// subagent's progress relay needs the pending tool call that the reducer is
/// about to complete.
pub struct Observation<'a> {
    /// The clock this message will be reduced under — the same value the
    /// recorder writes, so a replay of the log reproduces this step exactly.
    pub now: DateTime<Local>,
    pub msg: &'a Msg,
    pub state: &'a State,
}

/// A hook on every message an [`Engine`] pumps.
///
/// Synchronous and non-blocking: observers that emit events or progress to
/// channels (such as the subagent progress relay or broadcast streams) use
/// non-blocking channels to ensure observer dispatch never blocks or adds
/// latency to the reduction pump.
pub trait StepObserver {
    fn observe(&mut self, obs: Observation<'_>);
}

/// The no-op observer, for drivers that only want the reducer pumped.
impl StepObserver for () {
    fn observe(&mut self, _obs: Observation<'_>) {}
}

/// What one reduction did that a driver has to act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepOutcome {
    /// `state.should_exit` after the step. Every driver stops on it.
    pub should_exit: bool,
}

/// When [`Engine::drive`] returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriveExit {
    /// The turn went idle with nothing queued ([`StopWhen::Settled`]).
    Settled,
    /// The reducer asked to quit (`state.should_exit`).
    Exited,
    /// The cancel token fired. Under [`OnCancel::Unwind`] this also covers the
    /// grace window expiring before the turn finished unwinding.
    Cancelled,
    /// The wall-clock deadline elapsed.
    TimedOut,
    /// The message channel closed — the effect runner is gone.
    Closed,
}

/// How far [`Engine::drive`] runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopWhen {
    /// Run until the reducer says quit. The interactive session.
    Exit,
    /// Also stop once the turn is idle and no prompts are queued. Every
    /// headless driver: one prompt in, one answer out.
    Settled,
}

/// What a fired cancel token does to the drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnCancel {
    /// Stop the drive immediately. The caller still owns the state and is
    /// expected to shut its sink down. The subagent's child token.
    Abort,
    /// Inject `Msg::CancelTurn` — the same message the TUI's Esc sends — and
    /// keep pumping so the turn unwinds gracefully (tool process trees killed,
    /// the turn's `JoinSet` drained). Queued prompts must not seed another
    /// turn, so from then on the drive stops as soon as the turn is idle,
    /// drained or not. Hard-stops if `grace` elapses first.
    Unwind { grace: Duration },
}

/// Everything that ends a drive, in one value.
#[derive(Debug, Clone)]
pub struct DrivePolicy {
    pub stop: StopWhen,
    pub cancel: Option<CancellationToken>,
    pub on_cancel: OnCancel,
    /// Wall-clock budget for this drive. A `select!` arm rather than a
    /// `timeout()` wrapper, so a timed-out caller keeps its state and still
    /// reaches its own shutdown path instead of dropping the sink mid-flight
    /// (#76).
    pub deadline: Option<Duration>,
}

impl DrivePolicy {
    /// Run until the reducer quits. No cancel token, no deadline.
    #[must_use]
    pub const fn until_exit() -> Self {
        Self {
            stop: StopWhen::Exit,
            cancel: None,
            on_cancel: OnCancel::Abort,
            deadline: None,
        }
    }

    /// Run one turn to completion: stop when idle with nothing queued.
    #[must_use]
    pub const fn until_settled() -> Self {
        Self {
            stop: StopWhen::Settled,
            cancel: None,
            on_cancel: OnCancel::Abort,
            deadline: None,
        }
    }

    #[must_use]
    pub fn cancel_with(mut self, token: Option<CancellationToken>, on_cancel: OnCancel) -> Self {
        self.cancel = token;
        self.on_cancel = on_cancel;
        self
    }

    #[must_use]
    pub const fn deadline(mut self, deadline: Option<Duration>) -> Self {
        self.deadline = deadline;
        self
    }
}

/// The messages a drive pumps.
///
/// Two sources because that is how many exist: the effect runner's channel, and
/// (for the drivers that own the process) OS lifecycle signals. Terminal events
/// are deliberately absent — the interactive loop keeps its own `select!` for
/// those, because the `$EDITOR` round-trip has to drop and rebuild the event
/// stream around a suspend.
pub struct Inbox<'a> {
    msgs: &'a mut mpsc::Receiver<Msg>,
    lifecycle: Option<&'a mut RuntimeLifecycle>,
}

impl<'a> Inbox<'a> {
    #[must_use]
    pub const fn new(msgs: &'a mut mpsc::Receiver<Msg>) -> Self {
        Self {
            msgs,
            lifecycle: None,
        }
    }

    /// Merge OS lifecycle signals (SIGINT/SIGTERM/SIGHUP) into the stream, so
    /// an externally delivered signal unwinds through the reducer like `/quit`.
    #[must_use]
    pub const fn with_lifecycle(mut self, lifecycle: &'a mut RuntimeLifecycle) -> Self {
        self.lifecycle = Some(lifecycle);
        self
    }

    /// Next message, or `None` once the effect channel closes.
    ///
    /// A closed *lifecycle* channel is not the end of the inbox: it is dropped
    /// from the select and the effect channel carries on. (The predecessor
    /// `continue`d instead, which would have spun hot on a closed signal
    /// channel — unreachable in practice only because the signal tasks hold
    /// their sender for the life of the process.)
    async fn next(&mut self) -> Option<Msg> {
        loop {
            let Some(lifecycle) = self.lifecycle.as_mut() else {
                return self.msgs.recv().await;
            };
            tokio::select! {
                m = self.msgs.recv() => return m,
                s = lifecycle.next_msg() => match s {
                    Some(s) => return Some(s),
                    None => self.lifecycle = None,
                },
            }
        }
    }
}

/// The reducer, its state, and where its commands go.
pub struct Engine<S: EffectSink, O: StepObserver = ()> {
    /// `Option` only because `update` consumes `State` by value — the pure
    /// reducer's signature, and not up for negotiation. [`Engine::reduce`]
    /// takes it out and puts the new one back; it is `Some` at every
    /// observable point.
    state: Option<State>,
    sink: S,
    observer: O,
}

impl<S: EffectSink> Engine<S, ()> {
    /// An engine that pumps the reducer and nothing else.
    pub const fn new(state: State, sink: S) -> Self {
        Self {
            state: Some(state),
            sink,
            observer: (),
        }
    }
}

impl<S: EffectSink, O: StepObserver> Engine<S, O> {
    /// Attach something that watches every message before the reducer sees it.
    pub fn with_observer<O2: StepObserver>(self, observer: O2) -> Engine<S, O2> {
        Engine {
            state: self.state,
            sink: self.sink,
            observer,
        }
    }

    /// The current reducer state.
    ///
    /// # Panics
    ///
    /// Only if an earlier reduction panicked inside the reducer, which is what
    /// would leave the engine without a state. Every use of the engine after
    /// that is already a bug.
    #[must_use]
    pub const fn state(&self) -> &State {
        Self::present(self.state.as_ref())
    }

    /// Direct state access for the bootstrap window — seeding a conversation,
    /// stamping a scratchpad path — before the first message is pumped.
    ///
    /// # Panics
    ///
    /// As [`Engine::state`].
    pub const fn state_mut(&mut self) -> &mut State {
        self.state
            .as_mut()
            .expect("engine state is present between reductions")
    }

    pub const fn sink_mut(&mut self) -> &mut S {
        &mut self.sink
    }

    /// Give everything back, so the caller can build its result, seal whatever
    /// its observer was writing, and shut the runner down.
    ///
    /// # Panics
    ///
    /// As [`Engine::state`].
    pub fn into_parts(self) -> (State, S, O) {
        (
            self.state
                .expect("engine state is present between reductions"),
            self.sink,
            self.observer,
        )
    }

    /// No turn in flight.
    #[must_use]
    pub const fn is_idle(&self) -> bool {
        matches!(self.state().turn, TurnState::Idle)
    }

    /// Idle, with no user prompt waiting to seed the next turn. What every
    /// headless driver means by "done".
    #[must_use]
    pub fn is_settled(&self) -> bool {
        self.is_idle() && self.state().ui.queued_messages.is_empty()
    }

    /// The `Option`'s contract, spelled once: it is `Some` at every observable
    /// point. Takes the borrowed field rather than `&self` so callers that
    /// also need `&mut self.observer` can split the borrow.
    const fn present(state: Option<&State>) -> &State {
        state.expect("engine state is present between reductions")
    }

    /// Take the state out for the instant `update` owns it. Private, because
    /// the hole it leaves is never observable: `reduce` puts the new state back
    /// before it returns, and there is no `.await` in between.
    const fn take_state(&mut self) -> State {
        self.state
            .take()
            .expect("engine state is present between reductions")
    }

    /// The kernel: stamp the clock, reduce, route the commands.
    ///
    /// Synchronous and observer-free — `--replay` folds an entire recorded log
    /// through this with no runtime, stamping each entry's recorded timestamp
    /// instead of reading a clock.
    pub fn reduce(&mut self, now: DateTime<Local>, msg: Msg) -> StepOutcome {
        let mut state = self.take_state();
        // Inject the wall clock as data: the reducer never reads one, which is
        // what makes the same log fold to the same state tomorrow.
        state.now = now;
        let (state, cmds) = update(state, msg);
        let should_exit = state.should_exit;
        self.state = Some(state);
        for cmd in cmds {
            self.sink.dispatch(cmd);
        }
        StepOutcome { should_exit }
    }

    /// One message under the current wall clock, shown to the observer first.
    pub fn step(&mut self, msg: Msg) -> StepOutcome {
        self.step_at(Local::now(), msg)
    }

    /// [`Engine::step`] with the clock supplied by the caller — used where a
    /// single timestamp is shared with something else, such as the recorder
    /// line that must carry the exact `now` its message was reduced under.
    pub fn step_at(&mut self, now: DateTime<Local>, msg: Msg) -> StepOutcome {
        self.notify(now, &msg);
        self.reduce(now, msg)
    }

    /// Show one message to the observer, borrowing rather than owning it.
    ///
    /// Split out from [`Engine::step_at`] so `drive` can call it directly:
    /// keeping one `Msg`-sized slot out of the drive loop's future.
    fn notify(&mut self, now: DateTime<Local>, msg: &Msg) {
        // Split borrow: the observation reads `self.state`, the observe call
        // takes `self.observer` mutably. Disjoint fields, so both are fine.
        let obs = Observation {
            now,
            msg,
            state: Self::present(self.state.as_ref()),
        };
        self.observer.observe(obs);
    }

    /// Pump `inbox` until `policy` says stop.
    ///
    /// The `select!` is `biased`: cancellation and the deadline are one-shot
    /// arms that must win against a saturated message channel. (The interactive
    /// loop's fairness requirement — #112, where a hot channel starved terminal
    /// input — does not apply here, because there is no input arm to starve.)
    pub async fn drive(&mut self, inbox: &mut Inbox<'_>, policy: &DrivePolicy) -> DriveExit {
        let deadline = policy.deadline.map(|d| Instant::now() + d);
        // Set when a cancel token fires under `OnCancel::Unwind`: from then on
        // the drive stops as soon as the turn is idle (a queued prompt must not
        // seed another turn), or when this grace deadline passes.
        let mut unwind_by: Option<Instant> = None;

        loop {
            if matches!(policy.on_cancel, OnCancel::Abort)
                && policy
                    .cancel
                    .as_ref()
                    .is_some_and(CancellationToken::is_cancelled)
            {
                return DriveExit::Cancelled;
            }
            if matches!(policy.stop, StopWhen::Settled)
                && self.is_idle()
                && (self.state().ui.queued_messages.is_empty() || unwind_by.is_some())
            {
                return if unwind_by.is_some() {
                    DriveExit::Cancelled
                } else {
                    DriveExit::Settled
                };
            }

            // `select!` evaluates every branch expression even when its `if`
            // guard is false, so both sleep targets must be total.
            let far_future = || Instant::now() + Duration::from_secs(86_400);
            let msg = tokio::select! {
                biased;
                () = async {
                    match &policy.cancel {
                        Some(token) => token.cancelled().await,
                        None => std::future::pending().await,
                    }
                }, if policy.cancel.is_some() && unwind_by.is_none() => {
                    match policy.on_cancel {
                        OnCancel::Abort => return DriveExit::Cancelled,
                        OnCancel::Unwind { grace } => {
                            unwind_by = Some(Instant::now() + grace);
                            Msg::CancelTurn
                        },
                    }
                },
                () = tokio::time::sleep_until(unwind_by.unwrap_or_else(far_future)),
                    if unwind_by.is_some() => {
                    tracing::warn!("cancelled run did not unwind within grace; hard-stopping");
                    return DriveExit::Cancelled;
                },
                () = tokio::time::sleep_until(deadline.unwrap_or_else(far_future)),
                    if deadline.is_some() => return DriveExit::TimedOut,
                m = inbox.next() => match m {
                    Some(m) => m,
                    None => return DriveExit::Closed,
                },
            };

            // `notify` + `reduce` rather than `step`, to keep one `Msg`-sized
            // slot out of this loop's future — see `notify`.
            let now = Local::now();
            self.notify(now, &msg);
            if self.reduce(now, msg).should_exit {
                return DriveExit::Exited;
            }
        }
    }
}

#[cfg(test)]
mod tests;
