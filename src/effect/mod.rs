//! The effect runner: dispatches `Cmd` values into tokio tasks.
//!
//! There are exactly two places in the v0.7 codebase that spawn a
//! tokio task: this module and tests. Everywhere else asks the
//! reducer to return a `Cmd`, and the runner handles it. That
//! centralization is what makes structured concurrency per turn
//! actually work — nothing can accidentally spawn a detached task
//! that outlives the turn it was started for.
//!
//! Architecture:
//!
//! ```text
//!   main loop ── reducer ── Cmd ── dispatch ── EffectRunner
//!                                                 ├── TurnScope(turn A) ── JoinSet
//!                                                 ├── TurnScope(turn B) ── JoinSet
//!                                                 └── detached effects (Save, Exit, …)
//!                                                       ↓
//!                                              Msg via mpsc::Sender<Msg>
//!                                                       ↓
//!                                                 main loop (next iteration)
//! ```
//!
//! For commit 2 this file ships the **scaffold**: `EffectRunner`
//! exists, dispatches every `Cmd` variant, manages `TurnScope`
//! lifecycles, and emits placeholder Msgs where the real handler will
//! live. Commits 3–5 fill in the actual ModelProvider / ToolExecutor
//! bodies; commit 8 wires the runner into a real main loop.

mod middleware;
mod turn_scope;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::domain::{Cmd, Msg, TurnId};

pub use middleware::{DEFAULT_MAX_ATTEMPTS, retry_transient_http};
pub use turn_scope::TurnScope;

/// Single channel back to the reducer. `EffectRunner` holds the
/// sender; every spawned task clones this so it can emit `Msg` as
/// work progresses. Bounded capacity applies natural backpressure —
/// if the main loop can't keep up, the provider's streaming send
/// `.await`s and the whole pipeline throttles.
pub type MsgSender = mpsc::Sender<Msg>;

/// Bounded channel capacity for the effect → reducer stream. 512 is
/// generous — a single streaming chunk fits comfortably, and the
/// main loop drains at ~60 Hz so backlog rarely grows. Bigger wastes
/// RAM; smaller introduces spurious backpressure on bursty tool
/// output.
pub const MSG_CHANNEL_CAPACITY: usize = 512;

/// The runner. One instance per process, constructed by
/// `app::run` and consumed when the main loop exits.
pub struct EffectRunner {
    msg_tx: MsgSender,
    /// Per-turn scopes. Populated lazily: the first `Cmd` bearing a
    /// TurnId creates a scope; `Cmd::CancelScope` tears it down. A
    /// scope is also auto-reaped when its JoinSet drains empty, so
    /// completed turns don't accumulate.
    scopes: HashMap<TurnId, TurnScope>,
    /// Detached work (saves, persists, MCP lifecycle) lives here.
    /// This one set never gets cancelled piecemeal — shutdown drains
    /// it during `EffectRunner::shutdown`.
    detached: tokio::task::JoinSet<()>,
    /// MCP manager handle is held elsewhere (`crate::mcp` has a
    /// `OnceLock` for its global manager); we just note workdir so
    /// handlers can construct absolute paths.
    _workdir: PathBuf,
}

impl EffectRunner {
    /// Create an unused runner. Pair with `msg_rx` from `channel()`.
    pub fn new(msg_tx: MsgSender, workdir: PathBuf) -> Self {
        Self {
            msg_tx,
            scopes: HashMap::new(),
            detached: tokio::task::JoinSet::new(),
            _workdir: workdir,
        }
    }

    /// Pair-constructor: returns both the runner and the receiving
    /// end of the Msg channel. Preferred for production wiring
    /// because it keeps the channel capacity constant in one place.
    pub fn pair(workdir: PathBuf) -> (Self, mpsc::Receiver<Msg>) {
        let (tx, rx) = mpsc::channel(MSG_CHANNEL_CAPACITY);
        (Self::new(tx, workdir), rx)
    }

    /// Get or create the scope for a turn. Idempotent. The scope is
    /// retained until `CancelScope` tears it down or it naturally
    /// drains.
    fn scope_mut(&mut self, turn: TurnId) -> &mut TurnScope {
        self.scopes.entry(turn).or_insert_with(|| TurnScope::new(turn))
    }

    /// Drop the scope for a turn, signalling cancellation to every
    /// child first. Safe to call for non-existent turns.
    fn drop_scope(&mut self, turn: TurnId) {
        if let Some(scope) = self.scopes.remove(&turn) {
            scope.cancel();
            // Scope is dropped here → JoinSet is dropped → every
            // child task is aborted. Cooperative cancellation via the
            // token should already have unwound them at their next
            // await, so this is just the belt-and-suspenders.
            drop(scope);
        }
    }

    /// Number of active per-turn scopes. Tests use this to observe
    /// lifecycle without racing on internal state.
    pub fn scope_count(&self) -> usize {
        self.scopes.len()
    }

    /// Route a single `Cmd` into the appropriate spawn + handler.
    /// Returns immediately; handlers work asynchronously and emit
    /// `Msg` back through the sender channel.
    pub fn dispatch(&mut self, cmd: Cmd) {
        tracing::trace!(cmd = %cmd.summary(), "effect: dispatch");

        match cmd {
            Cmd::CallModel { turn, request } => {
                let tx = self.msg_tx.clone();
                let scope = self.scope_mut(turn);
                let token = scope.token();
                scope.spawn(async move {
                    // Scaffold: real dispatch lives in commit 3 when
                    // ModelProvider is introduced. For now we emit an
                    // UpstreamError so any caller wiring this runner
                    // into the reducer observes a well-formed end of
                    // turn rather than hanging forever.
                    tokio::select! {
                        _ = token.cancelled() => {
                            tracing::trace!(turn = %turn, "call_model cancelled (scaffold)");
                        },
                        _ = tokio::time::sleep(std::time::Duration::from_millis(1)) => {
                            let _ = request; // consume
                            let error = crate::models::UserFacingError {
                                summary: "not wired".to_string(),
                                message: "CallModel dispatch is scaffold-only in this commit; \
                                          real adapter lands in C3+".to_string(),
                                suggestion: "wait for v0.7.0 release".to_string(),
                                category: crate::models::ErrorCategory::Internal,
                                recoverable: false,
                            };
                            let _ = tx
                                .send(Msg::UpstreamError { turn, error })
                                .await;
                        },
                    }
                });
            },
            Cmd::ExecuteTool {
                turn,
                call_id,
                source,
            } => {
                let tx = self.msg_tx.clone();
                let scope = self.scope_mut(turn);
                let token = scope.token();
                scope.spawn(async move {
                    // Scaffold — real ToolExecutor dispatch lands in C5.
                    let _ = tx
                        .send(Msg::ToolStarted { turn, call_id })
                        .await;
                    tokio::select! {
                        _ = token.cancelled() => {
                            let _ = tx
                                .send(Msg::ToolFinished {
                                    turn,
                                    call_id,
                                    outcome: crate::domain::ToolOutcome::Cancelled,
                                })
                                .await;
                        },
                        _ = tokio::time::sleep(std::time::Duration::from_millis(1)) => {
                            let _ = source;
                            let _ = tx
                                .send(Msg::ToolFinished {
                                    turn,
                                    call_id,
                                    outcome: crate::domain::ToolOutcome::Error {
                                        error: "ExecuteTool is scaffold-only in this commit"
                                            .to_string(),
                                        duration_secs: 0.0,
                                    },
                                })
                                .await;
                        },
                    }
                });
            },
            Cmd::SpawnSubagents { turn, specs: _ } => {
                let tx = self.msg_tx.clone();
                let scope = self.scope_mut(turn);
                scope.spawn(async move {
                    let _ = tx; // unused in scaffold
                });
            },
            Cmd::CancelScope(turn) => {
                self.drop_scope(turn);
            },
            Cmd::SaveConversation(_) => {
                // Detached — persistence isn't turn-scoped. Emits
                // `SessionSaved` when complete.
                let tx = self.msg_tx.clone();
                self.detached.spawn(async move {
                    let _ = tx.send(Msg::SessionSaved).await;
                });
            },
            Cmd::PersistLastModel(_)
            | Cmd::PersistDefaultReasoning(_)
            | Cmd::PersistReasoningFor { .. } => {
                // Placeholder — real fs writes in C6.
            },
            Cmd::RefreshInstructions => {
                let tx = self.msg_tx.clone();
                self.detached.spawn(async move {
                    // Placeholder — real walk+refresh in C6.
                    let _ = tx.send(Msg::InstructionsChanged(None)).await;
                });
            },
            Cmd::LoadConversation(_) => {
                // Real impl in C6. No-op for now.
            },
            Cmd::InitMcpServers(_) => {
                // Real impl wraps the existing `McpServerManager` in C5.
            },
            Cmd::StopMcpServer { name } => {
                let tx = self.msg_tx.clone();
                self.detached.spawn(async move {
                    let _ = tx.send(Msg::McpServerStopped { name }).await;
                });
            },
            Cmd::PullOllamaModel { model } => {
                let tx = self.msg_tx.clone();
                self.detached.spawn(async move {
                    let _ = tx.send(Msg::ModelPullFinished { model }).await;
                });
            },
            Cmd::OpenInSystem(_) => {
                // Fire and forget via `crate::utils::open` in C6.
            },
            Cmd::DismissStatusAfter { ms } => {
                let tx = self.msg_tx.clone();
                self.detached.spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                    let _ = tx.send(Msg::StatusDismiss).await;
                });
            },
            Cmd::WriteImageToTemp { .. } => {
                // Real impl in C6.
            },
            Cmd::Exit => {
                // The main loop observes `state.should_exit` after
                // the reducer returns; the runner doesn't need to
                // take any special action. Documented here for
                // exhaustiveness.
            },
            Cmd::CancelSubagent { turn, subagent } => {
                // Scaffold — per-subagent cancellation wires into
                // `effect::subagent` in C5.
                let tx = self.msg_tx.clone();
                self.detached.spawn(async move {
                    let _ = tx
                        .send(Msg::SubagentStatusChanged {
                            turn,
                            subagent,
                            status: crate::domain::SubagentStatus::Cancelled,
                        })
                        .await;
                });
            },
        }
    }

    /// Async shutdown: cancel every scope, then wait for all spawned
    /// work to drain. Bounded by 5 seconds — a hung task past that
    /// gets aborted outright by `JoinSet::drop`.
    pub async fn shutdown(mut self) {
        for (id, scope) in self.scopes.iter() {
            tracing::debug!(turn = %id, "shutdown: cancelling scope");
            scope.cancel();
        }

        // Drain with a bounded timeout.
        let shutdown_deadline =
            tokio::time::Instant::now() + std::time::Duration::from_secs(5);

        let drain = async {
            for (_, mut scope) in self.scopes.drain() {
                scope.drain().await;
            }
            while let Some(result) = self.detached.join_next().await {
                if let Err(e) = result
                    && !e.is_cancelled()
                {
                    tracing::warn!(error = %e, "shutdown: detached task panic");
                }
            }
        };

        let _ = tokio::time::timeout_at(shutdown_deadline, drain).await;
    }

    /// Test helper: clone the Msg sender so a test can synthesize a
    /// message as if it came from an effect handler.
    #[doc(hidden)]
    pub fn msg_sender(&self) -> MsgSender {
        self.msg_tx.clone()
    }
}

/// Effect runner trait so nested reducers (subagents) can supply
/// their own. Production always uses the concrete `EffectRunner`.
#[async_trait::async_trait]
pub trait Runner: Send + Sync {
    fn dispatch(&self, cmd: Cmd);
}

/// Shim that wraps a `Mutex<EffectRunner>` for trait use. Not used in
/// the production path — reserved for nested subagent reducers in C5.
pub struct SharedRunner {
    inner: Arc<tokio::sync::Mutex<EffectRunner>>,
}

impl SharedRunner {
    pub fn new(runner: EffectRunner) -> Self {
        Self {
            inner: Arc::new(tokio::sync::Mutex::new(runner)),
        }
    }
}

#[async_trait::async_trait]
impl Runner for SharedRunner {
    fn dispatch(&self, cmd: Cmd) {
        let inner = self.inner.clone();
        tokio::spawn(async move {
            inner.lock().await.dispatch(cmd);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{SubagentId, ToolCallId};
    use std::time::Duration;

    fn runner() -> (EffectRunner, mpsc::Receiver<Msg>) {
        EffectRunner::pair(PathBuf::from("/tmp"))
    }

    #[tokio::test]
    async fn dispatch_exit_is_noop_on_runner_state() {
        let (mut r, _rx) = runner();
        r.dispatch(Cmd::Exit);
        assert_eq!(r.scope_count(), 0);
    }

    #[tokio::test]
    async fn dispatch_save_emits_session_saved() {
        let (mut r, mut rx) = runner();
        r.dispatch(Cmd::SaveConversation(
            crate::session::ConversationHistory::new("/p".to_string(), "m".to_string()),
        ));
        let msg = tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .expect("sender emits")
            .expect("channel alive");
        assert!(matches!(msg, Msg::SessionSaved));
    }

    #[tokio::test]
    async fn dispatch_dismiss_after_delay_emits_status_dismiss() {
        let (mut r, mut rx) = runner();
        let t0 = std::time::Instant::now();
        r.dispatch(Cmd::DismissStatusAfter { ms: 30 });
        let msg = tokio::time::timeout(Duration::from_millis(300), rx.recv())
            .await
            .expect("sender emits")
            .expect("channel alive");
        assert!(matches!(msg, Msg::StatusDismiss));
        assert!(t0.elapsed() >= Duration::from_millis(25));
    }

    #[tokio::test]
    async fn dispatch_call_model_creates_scope() {
        let (mut r, _rx) = runner();
        let turn = TurnId(7);
        let request = crate::domain::ChatRequest {
            model_id: "test/m".to_string(),
            messages: vec![],
            system_prompt: String::new(),
            instructions: None,
            reasoning: crate::models::ReasoningLevel::Medium,
            temperature: 0.7,
            max_tokens: 4096,
            tools: vec![],
        };
        r.dispatch(Cmd::CallModel { turn, request });
        assert_eq!(r.scope_count(), 1);
    }

    #[tokio::test]
    async fn dispatch_execute_tool_under_turn_emits_tool_started() {
        let (mut r, mut rx) = runner();
        let turn = TurnId(7);
        let call_id = ToolCallId(1);
        let source = crate::models::tool_call::ToolCall {
            id: Some("c1".to_string()),
            function: crate::models::tool_call::FunctionCall {
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "x"}),
            },
        };
        r.dispatch(Cmd::ExecuteTool {
            turn,
            call_id,
            source,
        });
        let first = tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .expect("some msg")
            .expect("channel alive");
        assert!(matches!(
            first,
            Msg::ToolStarted {
                turn: t,
                call_id: c,
            } if t == turn && c == call_id
        ));
    }

    #[tokio::test]
    async fn cancel_scope_before_execute_tool_drops_pending_work() {
        let (mut r, _rx) = runner();
        let turn = TurnId(9);
        r.dispatch(Cmd::CallModel {
            turn,
            request: crate::domain::ChatRequest {
                model_id: "m".to_string(),
                messages: vec![],
                system_prompt: String::new(),
                instructions: None,
                reasoning: crate::models::ReasoningLevel::Medium,
                temperature: 0.7,
                max_tokens: 4096,
                tools: vec![],
            },
        });
        assert_eq!(r.scope_count(), 1);

        r.dispatch(Cmd::CancelScope(turn));
        assert_eq!(r.scope_count(), 0);
    }

    #[tokio::test]
    async fn subagent_cancel_emits_status_changed() {
        let (mut r, mut rx) = runner();
        r.dispatch(Cmd::CancelSubagent {
            turn: TurnId(3),
            subagent: SubagentId(5),
        });
        let msg = tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .expect("msg")
            .expect("channel");
        match msg {
            Msg::SubagentStatusChanged {
                turn,
                subagent,
                status,
            } => {
                assert_eq!(turn, TurnId(3));
                assert_eq!(subagent, SubagentId(5));
                assert!(matches!(status, crate::domain::SubagentStatus::Cancelled));
            },
            _ => panic!("wrong variant"),
        }
    }

    #[tokio::test]
    async fn shutdown_drains_pending_saves() {
        let (mut r, _rx) = runner();
        for _ in 0..5 {
            r.dispatch(Cmd::SaveConversation(
                crate::session::ConversationHistory::new("/p".to_string(), "m".to_string()),
            ));
        }
        // Shutdown waits for all five to complete (should be instant).
        let start = std::time::Instant::now();
        r.shutdown().await;
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}
