//! Process-global handle to the `McpServerManager`.
//!
//! The manager itself owns the child processes and the JSON-RPC
//! client pool; it's constructed once at startup (by the effect
//! runner's `Cmd::InitMcpServers` handler) and lives for the rest
//! of the session. Everything else in the codebase — today just the
//! `McpToolProxy` in `providers::tool::mcp` — reaches for it through
//! `get()`.
//!
//! Why a `OnceLock` instead of threading through `State`? The
//! manager is an async runtime object (holds tokio tasks, child
//! stdin/stdout pipes, a pending-response map). It can't live inside
//! the pure reducer's `State`, and plumbing a fat handle through
//! every `ExecContext` just so one tool can reach it would bloat the
//! common path. Process-scope is the right lifetime and a single
//! global lookup is cheap.
//!
//! `MCP_READY_NOTIFY` exists for the narrow case where a tool call
//! races the startup path — a model that spews an `mcp__…` call on
//! the very first message before any of the servers have finished
//! initializing. Callers that want a blocking "wait for ready" use
//! `notified().await` before dispatching.

use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Notify;

use super::McpServerManager;

static MCP_MANAGER: OnceLock<Arc<McpServerManager>> = OnceLock::new();

/// True when MCP initialization is complete (either finished or not
/// configured). Starts true — the vast majority of sessions have no
/// MCP servers configured, and we don't want tool calls to block.
/// Flipped to false when `mark_init_started()` runs and back to true
/// when `mark_init_complete()` runs.
static MCP_INIT_COMPLETE: AtomicBool = AtomicBool::new(true);

/// Wakes any waiters parked in `wait_ready()` once init completes.
static MCP_READY_NOTIFY: Notify = Notify::const_new();

/// Install the manager. Called once, at startup, by the effect
/// runner. Idempotent — subsequent calls are dropped. Returns true
/// on the first (accepted) install for callers that want to know
/// whether they were the one that won the race.
pub fn set_manager(manager: Arc<McpServerManager>) -> bool {
    MCP_MANAGER.set(manager).is_ok()
}

/// Read the installed manager. `None` when nothing has called
/// `set_manager` yet — typically because no MCP servers are
/// configured, so the effect runner's init handler skipped the
/// install entirely.
pub fn get() -> Option<&'static Arc<McpServerManager>> {
    MCP_MANAGER.get()
}

/// Mark init as in-progress. Pair with `mark_init_complete` when
/// done. Races are fine — this is a hint for tool calls that want
/// to wait, not a synchronization primitive.
pub fn mark_init_started() {
    MCP_INIT_COMPLETE.store(false, Ordering::Release);
}

/// Mark init as complete and wake any waiters.
pub fn mark_init_complete() {
    MCP_INIT_COMPLETE.store(true, Ordering::Release);
    MCP_READY_NOTIFY.notify_waiters();
}

/// True iff init has finished (or was never started).
pub fn is_ready() -> bool {
    MCP_INIT_COMPLETE.load(Ordering::Acquire)
}

/// Await init completion. Returns immediately when `is_ready()`.
/// Otherwise parks on `MCP_READY_NOTIFY`.
pub async fn wait_ready() {
    // Enroll the waiter BEFORE checking readiness. `mark_init_complete` calls
    // `notify_waiters()`, which wakes only already-registered waiters and stores
    // no permit — so the naive "check is_ready(); then .notified().await" loses a
    // wakeup that lands between the two, leaving the waiter parked until its
    // caller's timeout. `Notified::enable()` registers without awaiting, closing
    // the window.
    let notified = MCP_READY_NOTIFY.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();
    if is_ready() {
        return;
    }
    notified.await;
}
