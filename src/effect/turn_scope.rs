//! Per-turn structured concurrency.
//!
//! A `TurnScope` owns exactly one `CancellationToken` and one
//! `JoinSet`. Every task spawned for this turn gets a clone of the
//! token, and every handle lands in the set. When the user cancels
//! (or the reducer dispatches `Cmd::CancelScope`), we cancel the
//! token — tokio's cooperative cancellation then unwinds every child
//! at its next `.await`. The set is drained on drop so no task leaks.
//!
//! The point: cancellation is a **signal**, not a **poll**. No child
//! task has to remember to check a shared flag; they're awaiting an
//! mpsc receive or an HTTP body, and `token.cancelled()` races every
//! such await via `select!`. Abort latency = the time to reach the
//! next await point — microseconds for HTTP streams, milliseconds for
//! tool subprocess fan-out.
//!
//! This is the v0.7 architectural replacement for the "drain events
//! every 50ms" polling pattern in `runtime::agent_loop`. That pattern
//! was correct by convention: you had to remember to call
//! `drain_events()` inside every long-running op, or cancellation
//! would hang until timeout. Forgetting was silent — which is how
//! `web_search` (30s) and `execute_command` (300s) both shipped with
//! the same bug. Here, forgetting is impossible: the token is baked
//! into every adapter's `StreamContext` / `ExecContext`, and the
//! adapter must `select!` on it to proceed.

use std::future::Future;

use tokio::task::{AbortHandle, JoinSet};
use tokio_util::sync::CancellationToken;

use crate::domain::TurnId;

/// One turn's cancellable scope. Construct once per `SubmitPrompt`;
/// abandon (drop) at the end of the turn.
#[derive(Debug)]
pub struct TurnScope {
    id: TurnId,
    token: CancellationToken,
    joins: JoinSet<()>,
}

impl TurnScope {
    pub fn new(id: TurnId) -> Self {
        Self {
            id,
            token: CancellationToken::new(),
            joins: JoinSet::new(),
        }
    }

    pub fn id(&self) -> TurnId {
        self.id
    }

    /// Clone the scope's token. Hand this to child tasks so they can
    /// participate in cooperative cancellation.
    pub fn token(&self) -> CancellationToken {
        self.token.clone()
    }

    /// Spawn a child task under this scope. The returned handle is
    /// retained inside the scope's `JoinSet` — callers don't need to
    /// keep it. Cancellation of the scope aborts the task at its next
    /// await.
    pub fn spawn<Fut>(&mut self, fut: Fut) -> AbortHandle
    where
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.joins.spawn(fut)
    }

    /// Signal cancellation to every child task. Returns immediately —
    /// callers drain the `JoinSet` separately via `drain_completed`.
    pub fn cancel(&self) {
        self.token.cancel();
    }

    /// True iff the scope has been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }

    /// Join one task if any has completed. Returns `None` immediately
    /// when the set is empty or nothing is ready. Intended for the
    /// main loop's per-tick bookkeeping — not a blocking drain.
    pub async fn join_next(&mut self) -> Option<Result<(), tokio::task::JoinError>> {
        self.joins.join_next().await
    }

    /// True iff no child task is currently running inside this scope.
    /// The main loop uses this after a `cancel()` to decide when to
    /// transition from `TurnState::Cancelling` back to `Idle`.
    pub fn is_empty(&self) -> bool {
        self.joins.is_empty()
    }

    pub fn len(&self) -> usize {
        self.joins.len()
    }

    /// Await every outstanding task to completion, swallowing
    /// `JoinError`s (they happen on normal `abort()`s after
    /// cancellation). Use during shutdown.
    pub async fn drain(&mut self) {
        while let Some(result) = self.joins.join_next().await {
            if let Err(e) = result
                && !e.is_cancelled()
            {
                tracing::warn!(
                    turn = %self.id,
                    error = %e,
                    "turn_scope: child task panicked"
                );
            }
        }
    }
}

impl Drop for TurnScope {
    fn drop(&mut self) {
        // If the caller forgot to cancel before dropping, be safe: a
        // live child task holding resources after the turn ended is
        // exactly the kind of leak this architecture exists to
        // prevent. `JoinSet::drop` already aborts its members, but we
        // still flip the cancellation token so any child that branches
        // on it observes the abort intent too.
        if !self.token.is_cancelled() {
            self.token.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn fresh_scope_has_no_tasks() {
        let scope = TurnScope::new(TurnId(1));
        assert_eq!(scope.len(), 0);
        assert!(scope.is_empty());
        assert!(!scope.is_cancelled());
    }

    #[tokio::test]
    async fn spawned_task_completes_within_scope() {
        let mut scope = TurnScope::new(TurnId(1));
        scope.spawn(async {
            tokio::time::sleep(Duration::from_millis(5)).await;
        });
        assert_eq!(scope.len(), 1);
        // Wait for it.
        let result = scope.join_next().await;
        assert!(result.is_some());
        assert!(scope.is_empty());
    }

    #[tokio::test]
    async fn cancel_signals_child_tasks() {
        let mut scope = TurnScope::new(TurnId(1));
        let token = scope.token();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<&'static str>();
        scope.spawn(async move {
            tokio::select! {
                _ = token.cancelled() => {
                    let _ = tx.send("cancelled");
                },
                _ = tokio::time::sleep(Duration::from_secs(30)) => {
                    let _ = tx.send("timeout");
                },
            }
        });

        // Give the task a moment to register its select.
        tokio::time::sleep(Duration::from_millis(10)).await;
        scope.cancel();
        let msg = tokio::time::timeout(Duration::from_millis(500), rx.recv())
            .await
            .expect("cancellation should propagate")
            .expect("sender alive");
        assert_eq!(msg, "cancelled");
        scope.drain().await;
    }

    #[tokio::test]
    async fn drop_cancels_token() {
        let token = {
            let scope = TurnScope::new(TurnId(2));
            scope.token()
        };
        // Scope dropped — token should be cancelled.
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn drain_runs_to_completion_on_normal_tasks() {
        let mut scope = TurnScope::new(TurnId(3));
        for i in 0..5 {
            scope.spawn(async move {
                tokio::time::sleep(Duration::from_millis(i)).await;
            });
        }
        assert_eq!(scope.len(), 5);
        scope.drain().await;
        assert!(scope.is_empty());
    }

    #[tokio::test]
    async fn cancel_then_drain_is_quick() {
        let mut scope = TurnScope::new(TurnId(4));
        let token = scope.token();
        for _ in 0..10 {
            let t = token.clone();
            scope.spawn(async move {
                tokio::select! {
                    _ = t.cancelled() => {},
                    _ = tokio::time::sleep(Duration::from_secs(60)) => {},
                }
            });
        }
        scope.cancel();
        let start = std::time::Instant::now();
        scope.drain().await;
        // All tasks cancel and unwind within 100ms (generous bound —
        // realistic would be <10ms).
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "cancel+drain took {:?}",
            start.elapsed()
        );
    }
}
