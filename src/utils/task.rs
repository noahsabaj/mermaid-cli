//! Task-ownership helpers.
//!
//! A sibling relay task must not outlive its parent future as a detached task.
//! [`spawn_guarded`] wraps a spawned task in an [`AbortOnDrop`] guard, so if the
//! parent is dropped (e.g. its turn is cancelled) before it `take()`s the handle
//! to await it, the task is aborted rather than leaked. The effect runner's
//! streaming relays and the provider stream bridge both own their relay tasks
//! this way, making the "every task is owned" invariant hold structurally rather
//! than only behaviorally (#F39, #58, #60).

/// Await a sibling relay task's handle, logging a panic (but not a normal
/// post-cancellation abort). Awaiting the handle keeps a stray panic from
/// vanishing the way a bare `let _ = handle.await` would.
pub(crate) async fn join_logged(handle: tokio::task::JoinHandle<()>, what: &str) {
    if let Err(e) = handle.await
        && !e.is_cancelled()
    {
        tracing::warn!(error = %e, task = what, "sibling relay task panicked");
    }
}

/// Owns a sibling relay task so it cannot outlive its parent future as a
/// detached task: if the parent is dropped before it `take()`s the handle for
/// [`join_logged`], the guard aborts the task on drop. On the normal path the
/// handle is taken and awaited, so the drop is a no-op.
pub(crate) struct AbortOnDrop(Option<tokio::task::JoinHandle<()>>);

impl AbortOnDrop {
    pub(crate) fn take(mut self) -> tokio::task::JoinHandle<()> {
        self.0.take().expect("AbortOnDrop::take called once")
    }
}

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if let Some(handle) = &self.0 {
            handle.abort();
        }
    }
}

/// `tokio::spawn` a relay, wrapped in an [`AbortOnDrop`] so the parent owns it.
pub(crate) fn spawn_guarded<F>(fut: F) -> AbortOnDrop
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    AbortOnDrop(Some(tokio::spawn(fut)))
}
