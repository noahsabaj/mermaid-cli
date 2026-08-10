//! The mailbox and the event bus of a running engine.
//!
//! [`Engine::drive`](super::Engine::drive) is already an actor loop: it pumps a
//! channel of `Msg` and publishes through its observer. What was missing was a
//! name for the two ends, so something outside the drive — a daemon socket, a
//! second view of one session, an SDK client — can reach it. A handle is that
//! name, and it is deliberately thin: both ends already existed, unnamed and
//! reachable only by whoever happened to hold the raw channel.
//!
//! The mailbox is the SAME channel every effect result arrives on, and that is
//! the point. A message sent from outside is indistinguishable from one the run
//! produced itself, so it goes through the same reducer, the same stale-turn
//! filter, the same recorder, and the same event log. There is no second way
//! into the state.

use std::fmt;

use tokio::sync::{broadcast, mpsc};

use mermaid_domain::Msg;

/// The drive that owned this handle's inbox has ended.
///
/// Carries the message back, because the caller is usually the one place that
/// can still do something with it — report it to a user, or persist it for the
/// next run. Boxed: `Msg` is the large enum this codebase already carries an
/// expect for, and an error that size rides every `Result` on the path.
#[derive(Debug)]
pub struct EngineGone(Box<Msg>);

impl EngineGone {
    /// The message that was not delivered.
    #[must_use]
    pub fn message(&self) -> &Msg {
        &self.0
    }

    /// Take the undelivered message back.
    #[must_use]
    pub fn into_message(self) -> Msg {
        *self.0
    }
}

impl fmt::Display for EngineGone {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the engine is no longer running")
    }
}

impl std::error::Error for EngineGone {}

/// A running engine, reachable from outside its drive.
///
/// Cheap to clone: both halves are channel senders. Cloning does not keep the
/// engine alive — the drive ends when its policy says so, and every handle then
/// reports [`EngineGone`].
pub struct EngineHandle<E> {
    inbox: mpsc::Sender<Msg>,
    events: broadcast::Sender<E>,
}

// Derived `Clone` would demand `E: Clone`, which is wrong: a `broadcast::Sender`
// clones regardless of what it carries.
impl<E> Clone for EngineHandle<E> {
    fn clone(&self) -> Self {
        Self {
            inbox: self.inbox.clone(),
            events: self.events.clone(),
        }
    }
}

impl<E> EngineHandle<E> {
    /// Wrap an existing pair. The daemon supplies its own event bus, because it
    /// has to subscribe *before* the run starts to catch the line that names
    /// the session.
    #[must_use]
    pub const fn new(inbox: mpsc::Sender<Msg>, events: broadcast::Sender<E>) -> Self {
        Self { inbox, events }
    }

    /// Wrap an inbox with a fresh event bus of `capacity`.
    #[must_use]
    pub fn with_capacity(inbox: mpsc::Sender<Msg>, capacity: usize) -> Self
    where
        E: Clone,
    {
        Self::new(inbox, broadcast::channel(capacity).0)
    }

    /// Deliver a message to the running engine.
    ///
    /// Awaits a slot when the inbox is full: that is backpressure from a run
    /// that is behind on its own effects, and dropping the message instead
    /// would lose a user's prompt. Use [`EngineHandle::try_send`] where the
    /// caller cannot wait.
    ///
    /// # Errors
    ///
    /// [`EngineGone`] once the drive has ended.
    pub async fn send(&self, msg: Msg) -> Result<(), EngineGone> {
        self.inbox
            .send(msg)
            .await
            .map_err(|e| EngineGone(Box::new(e.0)))
    }

    /// Deliver without waiting. Reports [`EngineGone`] for a finished engine
    /// and, for a full inbox, hands the message back the same way — a caller
    /// that cannot wait cannot queue either.
    ///
    /// # Errors
    ///
    /// [`EngineGone`] when the drive has ended or its inbox is full.
    pub fn try_send(&self, msg: Msg) -> Result<(), EngineGone> {
        use mpsc::error::TrySendError;
        match self.inbox.try_send(msg) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(m) | TrySendError::Closed(m)) => Err(EngineGone(Box::new(m))),
        }
    }

    /// Watch what the engine's observer publishes, from now on.
    ///
    /// A subscriber that needs what it missed reads the session event log first
    /// and then joins here — the shape `subscribe_task` already uses, and the
    /// reason this is a `broadcast` and not a replayable stream.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<E> {
        self.events.subscribe()
    }

    /// The publishing end, for whatever observer feeds this handle.
    #[must_use]
    pub const fn publisher(&self) -> &broadcast::Sender<E> {
        &self.events
    }

    /// Whether the drive is still pumping. Racy by nature — a run can end
    /// between the check and the send, which is why [`EngineHandle::send`]
    /// reports it too.
    #[must_use]
    pub fn is_running(&self) -> bool {
        !self.inbox.is_closed()
    }
}

impl<E> fmt::Debug for EngineHandle<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EngineHandle")
            .field("running", &self.is_running())
            .field("subscribers", &self.events.receiver_count())
            // What a reader wants is the state of the two ends, not the two
            // senders themselves, which print as nothing useful.
            .finish_non_exhaustive()
    }
}
