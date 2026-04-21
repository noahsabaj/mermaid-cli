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

use crate::app::Config;
use crate::domain::{Cmd, Msg, TurnId};
use crate::providers::ctx::{ExecContext, StreamContext};
use crate::providers::{ProviderFactory, StreamEvent, ToolRegistry};

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
    workdir: PathBuf,
    /// Lazy provider registry. `CallModel` resolves through this.
    /// `None` in the scaffold path (the C2 tests); production
    /// construction via `with_bindings` sets it.
    providers: Option<Arc<ProviderFactory>>,
    /// Shared tool registry. `None` in scaffold paths.
    tools: Option<Arc<ToolRegistry>>,
}

impl EffectRunner {
    /// Create an unused runner. Pair with `msg_rx` from `channel()`.
    pub fn new(msg_tx: MsgSender, workdir: PathBuf) -> Self {
        Self {
            msg_tx,
            scopes: HashMap::new(),
            detached: tokio::task::JoinSet::new(),
            workdir,
            providers: None,
            tools: None,
        }
    }

    /// Attach real provider + tool registries. The new main loop
    /// calls this after constructing the runner; the C2 scaffold
    /// tests don't. When absent, `CallModel` / `ExecuteTool` emit
    /// the `not wired` placeholder Msgs (useful for tests that
    /// don't care about real providers).
    pub fn with_bindings(
        mut self,
        providers: Arc<ProviderFactory>,
        tools: Arc<ToolRegistry>,
    ) -> Self {
        self.providers = Some(providers);
        self.tools = Some(tools);
        self
    }

    /// Pair-constructor: returns both the runner and the receiving
    /// end of the Msg channel. Preferred for production wiring
    /// because it keeps the channel capacity constant in one place.
    pub fn pair(workdir: PathBuf) -> (Self, mpsc::Receiver<Msg>) {
        let (tx, rx) = mpsc::channel(MSG_CHANNEL_CAPACITY);
        (Self::new(tx, workdir), rx)
    }

    /// Pair constructor that also wires the real provider factory +
    /// tool registry. Used by `app::run_v7`.
    pub fn pair_with_bindings(
        workdir: PathBuf,
        config: Config,
        tools: Arc<ToolRegistry>,
    ) -> (Self, mpsc::Receiver<Msg>) {
        let (tx, rx) = mpsc::channel(MSG_CHANNEL_CAPACITY);
        let providers = Arc::new(ProviderFactory::new(config));
        (Self::new(tx, workdir).with_bindings(providers, tools), rx)
    }

    /// Get or create the scope for a turn. Idempotent. The scope is
    /// retained until `CancelScope` tears it down or it naturally
    /// drains.
    fn scope_mut(&mut self, turn: TurnId) -> &mut TurnScope {
        self.scopes
            .entry(turn)
            .or_insert_with(|| TurnScope::new(turn))
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
                let providers = self.providers.clone();
                let scope = self.scope_mut(turn);
                let token = scope.token();
                scope.spawn(async move {
                    dispatch_call_model(tx, providers, turn, request, token).await;
                });
            },
            Cmd::ExecuteTool {
                turn,
                call_id,
                source,
            } => {
                let tx = self.msg_tx.clone();
                let tools = self.tools.clone();
                let workdir = self.workdir.clone();
                let scope = self.scope_mut(turn);
                let token = scope.token();
                scope.spawn(async move {
                    dispatch_execute_tool(tx, tools, workdir, turn, call_id, source, token).await;
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
            Cmd::SaveConversation(history) => {
                let tx = self.msg_tx.clone();
                let workdir = self.workdir.clone();
                self.detached.spawn(async move {
                    if let Ok(manager) = crate::session::ConversationManager::new(&workdir)
                        && manager.save_conversation(&history).is_ok()
                    {
                        let _ = tx.send(Msg::SessionSaved).await;
                    } else {
                        tracing::warn!("SaveConversation: failed to write to disk");
                    }
                });
            },
            Cmd::PersistLastModel(model) => {
                self.detached.spawn(async move {
                    let _ = crate::app::persist_last_model(&model);
                });
            },
            Cmd::PersistDefaultReasoning(level) => {
                self.detached.spawn(async move {
                    let _ = crate::app::persist_default_reasoning(level);
                });
            },
            Cmd::PersistReasoningFor { model_id, level } => {
                self.detached.spawn(async move {
                    let _ = crate::app::persist_reasoning_for_model(&model_id, level);
                });
            },
            Cmd::RefreshInstructions => {
                let tx = self.msg_tx.clone();
                let workdir = self.workdir.clone();
                self.detached.spawn(async move {
                    let (loaded, _outcome) = crate::app::instructions::refresh(None, &workdir);
                    let _ = tx.send(Msg::InstructionsChanged(loaded)).await;
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
        let shutdown_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);

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

/// Dispatch a `CallModel` command. If a `ProviderFactory` is bound,
/// resolves the provider and streams its events onto the Msg
/// channel. Without bindings (scaffold tests) falls back to a
/// placeholder `UpstreamError` so the reducer sees a clean end of
/// turn.
async fn dispatch_call_model(
    msg_tx: MsgSender,
    providers: Option<Arc<ProviderFactory>>,
    turn: TurnId,
    request: crate::domain::ChatRequest,
    token: tokio_util::sync::CancellationToken,
) {
    use crate::models::UserFacingError;

    let Some(factory) = providers else {
        let error = UserFacingError {
            summary: "not wired".to_string(),
            message: "EffectRunner has no ProviderFactory bound".to_string(),
            suggestion: "construct via EffectRunner::pair_with_bindings".to_string(),
            category: crate::models::ErrorCategory::Internal,
            recoverable: false,
        };
        let _ = msg_tx.send(Msg::UpstreamError { turn, error }).await;
        return;
    };

    // Lazily resolve the provider for this model.
    let provider = match factory.resolve(&request.model_id).await {
        Ok(p) => p,
        Err(e) => {
            let error = classify_error_for_ui(&e);
            let _ = msg_tx.send(Msg::UpstreamError { turn, error }).await;
            return;
        },
    };

    // Build a StreamContext — provider writes typed events into the
    // internal sink; we relay each to the reducer as a Msg.
    let (stream_tx, mut stream_rx) = mpsc::channel::<StreamEvent>(256);
    let ctx = StreamContext::new(token.clone(), stream_tx, turn);

    // Drain stream events into Msgs on a sibling task. Drops when
    // the sink closes (provider's final `Done` or cancel).
    let relay_tx = msg_tx.clone();
    let relay = tokio::spawn(async move {
        while let Some(event) = stream_rx.recv().await {
            let msg = match event {
                StreamEvent::Text(chunk) => Msg::StreamText { turn, chunk },
                StreamEvent::Reasoning(chunk) => Msg::StreamReasoning { turn, chunk },
                StreamEvent::ToolCall(call) => Msg::StreamToolCall { turn, call },
                StreamEvent::ThinkingSignature(_) => continue, // folded into Done below
                StreamEvent::Done {
                    usage,
                    thinking_signature,
                } => Msg::StreamDone {
                    turn,
                    usage,
                    thinking_signature,
                },
            };
            if relay_tx.send(msg).await.is_err() {
                break;
            }
        }
    });

    // Run the actual provider. On error, the relay will have
    // already emitted partial events; we follow with a single
    // UpstreamError to terminate the turn cleanly.
    match provider.chat(request, ctx).await {
        Ok(_final_response) => {
            // Success — the final `Done` flowed through the sink.
        },
        Err(e) => {
            let error = classify_error_for_ui(&e);
            let _ = msg_tx.send(Msg::UpstreamError { turn, error }).await;
        },
    }

    let _ = relay.await;
}

/// Dispatch an `ExecuteTool` command.
async fn dispatch_execute_tool(
    msg_tx: MsgSender,
    tools: Option<Arc<ToolRegistry>>,
    workdir: PathBuf,
    turn: TurnId,
    call_id: crate::domain::ToolCallId,
    source: crate::models::tool_call::ToolCall,
    token: tokio_util::sync::CancellationToken,
) {
    let _ = msg_tx.send(Msg::ToolStarted { turn, call_id }).await;

    let Some(registry) = tools else {
        let _ = msg_tx
            .send(Msg::ToolFinished {
                turn,
                call_id,
                outcome: crate::domain::ToolOutcome::Error {
                    error: "EffectRunner has no ToolRegistry bound".to_string(),
                    duration_secs: 0.0,
                },
            })
            .await;
        return;
    };

    // Route MCP-prefixed calls to the mcp proxy, which takes
    // {server_name, tool_name, arguments}. The raw model call has
    // those embedded in the function name and arguments respectively.
    let (tool_key, args) = if source.function.name.starts_with("mcp__") {
        let rest = &source.function.name[5..];
        if let Some((server, tool)) = rest.split_once("__") {
            (
                "mcp_proxy",
                serde_json::json!({
                    "server_name": server,
                    "tool_name": tool,
                    "arguments": source.function.arguments.clone(),
                }),
            )
        } else {
            let _ = msg_tx
                .send(Msg::ToolFinished {
                    turn,
                    call_id,
                    outcome: crate::domain::ToolOutcome::Error {
                        error: format!("invalid MCP tool name: {}", source.function.name),
                        duration_secs: 0.0,
                    },
                })
                .await;
            return;
        }
    } else {
        (
            source.function.name.as_str(),
            source.function.arguments.clone(),
        )
    };

    let Some(tool) = registry.get(tool_key) else {
        let _ = msg_tx
            .send(Msg::ToolFinished {
                turn,
                call_id,
                outcome: crate::domain::ToolOutcome::Error {
                    error: format!("unknown tool: {}", tool_key),
                    duration_secs: 0.0,
                },
            })
            .await;
        return;
    };

    let (progress_tx, _progress_rx) = mpsc::channel(16);
    let ctx = ExecContext::new(token, progress_tx, call_id, turn, workdir);
    let outcome = tool.execute(args, ctx).await;
    let _ = msg_tx
        .send(Msg::ToolFinished {
            turn,
            call_id,
            outcome,
        })
        .await;
}

fn classify_error_for_ui(e: &crate::models::ModelError) -> crate::models::UserFacingError {
    use crate::models::{ErrorCategory, ModelError, UserFacingError};
    match e {
        ModelError::Backend(b) => UserFacingError {
            summary: "Backend error".to_string(),
            message: b.to_string(),
            suggestion: "Check the provider endpoint / API key.".to_string(),
            category: ErrorCategory::Connection,
            recoverable: true,
        },
        ModelError::Authentication(msg) => UserFacingError {
            summary: "Auth error".to_string(),
            message: msg.clone(),
            suggestion: "Set the env var the provider expects.".to_string(),
            category: ErrorCategory::Auth,
            recoverable: false,
        },
        ModelError::RateLimit { retry_after } => UserFacingError {
            summary: "Rate limit".to_string(),
            message: format!("retry after {:?}", retry_after),
            suggestion: "Wait and try again.".to_string(),
            category: ErrorCategory::Temporary,
            recoverable: true,
        },
        ModelError::StreamError(msg) => UserFacingError {
            summary: "Stream error".to_string(),
            message: msg.clone(),
            suggestion: "Retry the request.".to_string(),
            category: ErrorCategory::Connection,
            recoverable: true,
        },
        other => UserFacingError {
            summary: "Model error".to_string(),
            message: other.to_string(),
            suggestion: String::new(),
            category: ErrorCategory::Internal,
            recoverable: false,
        },
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
