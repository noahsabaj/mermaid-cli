//! Process lifecycle signal handling.
//!
//! Crossterm raw mode turns a typed Ctrl+C into a key event, but OS
//! signals can still arrive from `kill`, terminal close, or a process
//! manager. This module converts those signals into reducer messages
//! so shutdown follows the same path as `/quit`.

use tokio::sync::mpsc;

use mermaid_domain::{Msg, RuntimeSignal};

/// Small signal stream consumed by the app main loops.
pub struct RuntimeLifecycle {
    rx: mpsc::UnboundedReceiver<RuntimeSignal>,
}

impl RuntimeLifecycle {
    #[must_use]
    pub fn new() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        spawn_signal_tasks(tx);
        Self { rx }
    }

    pub async fn next_msg(&mut self) -> Option<Msg> {
        self.rx.recv().await.map(Msg::RuntimeSignal)
    }

    /// A lifecycle whose signals a test delivers by hand. `new()` installs
    /// real OS handlers, which a test can neither fire nor close.
    #[cfg(test)]
    pub(crate) fn for_test() -> (mpsc::UnboundedSender<RuntimeSignal>, Self) {
        let (tx, rx) = mpsc::unbounded_channel();
        (tx, Self { rx })
    }
}

impl Default for RuntimeLifecycle {
    fn default() -> Self {
        Self::new()
    }
}

fn spawn_signal_tasks(tx: mpsc::UnboundedSender<RuntimeSignal>) {
    let ctrl_c_tx = tx.clone();
    tokio::spawn(async move {
        // Loop, not one-shot: a second Ctrl+C during a stalled shutdown must
        // still be delivered (the old task exited after the first signal, so a
        // wedged MCP-drain window made Ctrl+C appear dead). `ctrl_c()` re-arms on
        // each await; stop only if registration fails or the receiver is gone.
        loop {
            if tokio::signal::ctrl_c().await.is_err() {
                break;
            }
            if ctrl_c_tx.send(RuntimeSignal::Interrupt).is_err() {
                break; // app shutting down; nobody left to receive.
            }
        }
    });

    spawn_unix_signal_tasks(tx);
}

#[cfg(unix)]
fn spawn_unix_signal_tasks(tx: mpsc::UnboundedSender<RuntimeSignal>) {
    use tokio::signal::unix::{SignalKind, signal};

    let terminate_tx = tx.clone();
    tokio::spawn(async move {
        if let Ok(mut sigterm) = signal(SignalKind::terminate()) {
            while sigterm.recv().await.is_some() {
                if terminate_tx.send(RuntimeSignal::Terminate).is_err() {
                    break;
                }
            }
        }
    });

    tokio::spawn(async move {
        if let Ok(mut sighup) = signal(SignalKind::hangup()) {
            while sighup.recv().await.is_some() {
                if tx.send(RuntimeSignal::Hangup).is_err() {
                    break;
                }
            }
        }
    });
}

#[cfg(not(unix))]
fn spawn_unix_signal_tasks(_tx: mpsc::UnboundedSender<RuntimeSignal>) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lifecycle_wraps_signal_as_reducer_msg() {
        let (tx, mut lifecycle) = RuntimeLifecycle::for_test();
        tx.send(RuntimeSignal::Terminate).expect("send signal");

        let msg = lifecycle.next_msg().await.expect("signal msg");
        assert!(matches!(msg, Msg::RuntimeSignal(RuntimeSignal::Terminate)));
    }
}
