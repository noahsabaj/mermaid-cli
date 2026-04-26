//! The effect runner: dispatches `Cmd` values into tokio tasks.
//!
//! There are exactly two places in the codebase that spawn a tokio
//! task: this module and tests. Everywhere else asks the
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
//! The runner dispatches every `Cmd` variant to a real handler —
//! model streaming (`CallModel` → `ModelProvider::chat`), tool
//! execution (`ExecuteTool` → `ToolExecutor::execute`), persistence
//! (`SaveConversation`, `LoadConversation`, `PersistLastModel`,
//! `PersistReasoningFor`, `RefreshInstructions`), MCP lifecycle
//! (`InitMcpServers`, `StopMcpServer`), local side-effects
//! (`WriteImageToTemp`, `OpenInSystem`, `PullOllamaModel`,
//! `SetTerminalTitle`, `DismissStatusAfter`). Cancellation flows
//! through `Cmd::CancelScope(TurnId)` → the scope's
//! `CancellationToken`.

mod middleware;
mod turn_scope;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::mpsc;

use crate::app::Config;
use crate::domain::{
    Cmd, CompactionPolicy, CompactionRequest, CompactionResult, CompactionTrigger, Msg, TurnId,
};
use crate::models::{ModelError, TokenUsage};
use crate::providers::ctx::{ExecContext, StreamContext};
use crate::providers::model::ModelProvider;
use crate::providers::{ProviderFactory, StreamEvent, ToolRegistry};

pub use middleware::{DEFAULT_MAX_ATTEMPTS, retry_transient_http};
pub use turn_scope::TurnScope;

#[cfg(not(test))]
const CANCEL_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
#[cfg(test)]
const CANCEL_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(50);

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
    /// TurnId creates a scope; `Cmd::CancelScope` tears it down.
    /// Empty (drained) scopes are reaped by `reap_empty_scopes`, which
    /// runs at the top of every `dispatch` call so the map stays
    /// bounded across long sessions (F12).
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
    /// Tests that don't care about real providers leave this `None`
    /// and observe the fallback `UpstreamError` Msg; production
    /// construction via `with_bindings` sets it.
    providers: Option<Arc<ProviderFactory>>,
    /// Shared tool registry. See `providers` — same optionality
    /// rationale for unit tests.
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

    /// Attach provider + tool registries. Production wiring uses
    /// this; unit tests that don't need real dispatch can skip.
    /// Without bindings, `CallModel` / `ExecuteTool` emit well-
    /// formed error Msgs so the reducer still transitions cleanly.
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
    /// tool registry. Used by `app::run_interactive`.
    pub fn pair_with_bindings(
        workdir: PathBuf,
        config: Config,
        tools: Arc<ToolRegistry>,
    ) -> (Self, mpsc::Receiver<Msg>) {
        let providers = Arc::new(ProviderFactory::new(config));
        Self::pair_from(workdir, providers, tools)
    }

    /// Pair constructor that takes a pre-built `ProviderFactory`.
    /// Used when the caller needs to share a `ProviderFactory` with
    /// the `SubagentSpawner` so subagents can issue model calls
    /// through the same cache.
    pub fn pair_from(
        workdir: PathBuf,
        providers: Arc<ProviderFactory>,
        tools: Arc<ToolRegistry>,
    ) -> (Self, mpsc::Receiver<Msg>) {
        let (tx, rx) = mpsc::channel(MSG_CHANNEL_CAPACITY);
        (Self::new(tx, workdir).with_bindings(providers, tools), rx)
    }

    /// Construct a runner that shares a pre-derived cancellation
    /// token for its turn scopes. Used by `SubagentSpawner` so the
    /// child runner's work aborts as soon as the parent's `ctx.token`
    /// fires.
    pub fn new_child(
        msg_tx: MsgSender,
        workdir: PathBuf,
        providers: Arc<ProviderFactory>,
        tools: Arc<ToolRegistry>,
    ) -> Self {
        Self::new(msg_tx, workdir).with_bindings(providers, tools)
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
    ///
    /// After the scope is cancelled, a detached task moves it off the
    /// runner, drains its `JoinSet` (so child tasks unwind), then emits
    /// `Msg::TurnCancelled(turn)` so the reducer can transition
    /// `Cancelling → Idle`. Without this terminal event the TUI would
    /// stick in `Cancelling` — the reducer has no other way to learn
    /// that the abort fully landed.
    fn drop_scope(&mut self, turn: TurnId) {
        if let Some(mut scope) = self.scopes.remove(&turn) {
            scope.cancel();
            let tx = self.msg_tx.clone();
            self.detached.spawn(async move {
                if tokio::time::timeout(CANCEL_DRAIN_TIMEOUT, scope.drain())
                    .await
                    .is_err()
                {
                    tracing::warn!(
                        turn = %turn,
                        timeout_ms = CANCEL_DRAIN_TIMEOUT.as_millis(),
                        "cancel drain timed out; aborting remaining scoped tasks"
                    );
                }
                let _ = tx.send(Msg::TurnCancelled(turn)).await;
            });
        }
    }

    /// Number of active per-turn scopes. Tests use this to observe
    /// lifecycle without racing on internal state.
    pub fn scope_count(&self) -> usize {
        self.scopes.len()
    }

    /// F12: remove scope entries whose `JoinSet` is empty — every
    /// child task has completed, so the scope is just an orphan key
    /// in the map. Called at the top of `dispatch` so the map stays
    /// bounded over long sessions. Cheap: one linear walk, no async.
    ///
    /// `JoinSet::is_empty` only returns true after completed tasks are
    /// harvested via `join_next`/`try_join_next`, so we first drain
    /// any ready completions per scope.
    fn reap_empty_scopes(&mut self) {
        self.scopes.retain(|_, scope| {
            scope.drain_completed();
            !scope.is_empty()
        });
    }

    /// Route a single `Cmd` into the appropriate spawn + handler.
    /// Returns immediately; handlers work asynchronously and emit
    /// `Msg` back through the sender channel.
    pub fn dispatch(&mut self, cmd: Cmd) {
        // F12: reap any drained scopes before touching the map. Keeps
        // `scope_count()` bounded as the session grows.
        self.reap_empty_scopes();
        tracing::trace!(cmd = %cmd.summary(), "effect: dispatch");

        match cmd {
            Cmd::CallModel { turn, mut request } => {
                let tx = self.msg_tx.clone();
                let providers = self.providers.clone();
                // Enrich `request.tools` with every user-facing
                // tool in the bound registry. The reducer has
                // already populated MCP tools from `state.mcp`;
                // built-ins come from the runner (which holds the
                // registry). This keeps `ChatRequest.tools` the
                // single source of truth for what the model sees.
                if let Some(tools) = &self.tools {
                    let mut enriched = tools.describe_all();
                    enriched.append(&mut request.tools);
                    request.tools = enriched;
                }
                let scope = self.scope_mut(turn);
                let token = scope.token();
                scope.spawn(async move {
                    dispatch_call_model(tx, providers, turn, request, token).await;
                });
            },
            Cmd::CompactConversation { turn, mut request } => {
                let tx = self.msg_tx.clone();
                let providers = self.providers.clone();
                if let Some(tools) = &self.tools {
                    let mut enriched = tools.describe_all();
                    enriched.append(&mut request.chat.tools);
                    request.chat.tools = enriched;
                }
                let scope = self.scope_mut(turn);
                let token = scope.token();
                scope.spawn(async move {
                    dispatch_compact_conversation(tx, providers, turn, request, token).await;
                });
            },
            Cmd::ExecuteTool {
                turn,
                call_id,
                source,
                model_id,
            } => {
                let tx = self.msg_tx.clone();
                let tools = self.tools.clone();
                let workdir = self.workdir.clone();
                // Pass the shared Config from ProviderFactory so
                // subagents inherit it (F7). Falls back to
                // Config::default() when providers aren't bound (unit
                // tests without real wiring).
                let config = self
                    .providers
                    .as_ref()
                    .map(|p| Arc::new(p.config().clone()))
                    .unwrap_or_else(|| Arc::new(crate::app::Config::default()));
                let scope = self.scope_mut(turn);
                let token = scope.token();
                scope.spawn(async move {
                    dispatch_execute_tool(
                        tx, tools, workdir, turn, call_id, source, token, config, model_id,
                    )
                    .await;
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
            Cmd::SaveCompactionArchive(archive) => {
                let workdir = self.workdir.clone();
                self.detached.spawn(async move {
                    if let Ok(manager) = crate::session::ConversationManager::new(&workdir)
                        && let Err(err) = manager.save_compaction_archive(&archive)
                    {
                        tracing::warn!(error = %err, "SaveCompactionArchive: failed to write archive");
                    }
                });
            },
            Cmd::PersistLastModel(model) => {
                self.detached.spawn(async move {
                    let _ = crate::app::persist_last_model(&model);
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
            Cmd::LoadConversation(id) => {
                let tx = self.msg_tx.clone();
                let workdir = self.workdir.clone();
                self.detached.spawn(async move {
                    match crate::session::ConversationManager::new(&workdir) {
                        Ok(mgr) => match mgr.load_conversation(&id) {
                            Ok(history) => {
                                let _ = tx.send(Msg::ConversationLoaded(history)).await;
                            },
                            Err(e) => {
                                tracing::warn!(id = %id, error = %e, "LoadConversation failed");
                            },
                        },
                        Err(e) => {
                            tracing::warn!(error = %e, "ConversationManager init failed");
                        },
                    }
                });
            },
            Cmd::ListConversations => {
                let tx = self.msg_tx.clone();
                let workdir = self.workdir.clone();
                self.detached.spawn(async move {
                    let summaries = match crate::session::ConversationManager::new(&workdir) {
                        Ok(mgr) => mgr
                            .list_conversations()
                            .unwrap_or_default()
                            .into_iter()
                            .map(|h| crate::domain::ConversationSummary {
                                id: h.id.clone(),
                                title: h.title.clone(),
                                message_count: h.messages.len(),
                                updated_at: h.updated_at.to_rfc3339(),
                            })
                            .collect(),
                        Err(_) => Vec::new(),
                    };
                    let _ = tx.send(Msg::ConversationsListed(summaries)).await;
                });
            },
            Cmd::InitMcpServers(configs) => {
                let tx = self.msg_tx.clone();
                self.detached.spawn(async move {
                    if configs.is_empty() {
                        return;
                    }
                    crate::mcp::manager_ref::mark_init_started();
                    let manager =
                        std::sync::Arc::new(crate::mcp::McpServerManager::start(&configs).await);
                    // Emit a Ready or Errored per server based on
                    // what came up. Zero-tool servers are still ready
                    // if the manager has an active client for them.
                    for (name, _cfg) in configs.iter() {
                        let server_tools: Vec<crate::domain::McpToolSpec> = manager
                            .get_all_tools()
                            .iter()
                            .filter(|(server, _)| server == name)
                            .map(|(_, def)| crate::domain::McpToolSpec {
                                name: def.name.clone(),
                                description: def.description.clone(),
                                input_schema: def.input_schema.clone(),
                            })
                            .collect();
                        let msg = mcp_startup_msg(name, manager.has_server(name), server_tools);
                        let _ = tx.send(msg).await;
                    }
                    crate::mcp::manager_ref::set_manager(manager);
                    crate::mcp::manager_ref::mark_init_complete();
                });
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
                    dispatch_pull_ollama_model(tx, model).await;
                });
            },
            Cmd::OpenInSystem(path) => {
                self.detached.spawn(async move {
                    let _ = tokio::task::spawn_blocking(move || {
                        crate::utils::open_file(&path);
                    })
                    .await;
                });
            },
            Cmd::DismissStatusAfter { ms } => {
                let tx = self.msg_tx.clone();
                self.detached.spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                    let _ = tx.send(Msg::StatusDismiss).await;
                });
            },
            Cmd::WriteImageToTemp {
                path,
                bytes,
                format: _,
            } => {
                self.detached.spawn(async move {
                    if let Err(e) = tokio::fs::write(&path, &bytes).await {
                        tracing::warn!(path = %path.display(), error = %e, "WriteImageToTemp failed");
                    }
                });
            },
            Cmd::ReadClipboard => {
                let tx = self.msg_tx.clone();
                self.detached.spawn(async move {
                    dispatch_read_clipboard(tx).await;
                });
            },
            Cmd::Exit => {
                // The main loop observes `state.should_exit` after
                // the reducer returns; the runner doesn't need to
                // take any special action. Documented here for
                // exhaustiveness.
            },
            Cmd::SetTerminalTitle(title) => {
                self.detached.spawn(async move {
                    use std::io::Write;
                    let seq = format!("\x1b]2;{}\x07", title);
                    let mut stdout = std::io::stdout();
                    let _ = stdout.write_all(seq.as_bytes());
                    let _ = stdout.flush();
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

/// Dispatch a `CallModel` command. Resolves the provider (lazy,
/// cached) and streams its events onto the Msg channel. Without a
/// bound `ProviderFactory` (unit tests), emits a single
/// `UpstreamError` so the reducer ends the turn cleanly.
async fn dispatch_call_model(
    msg_tx: MsgSender,
    providers: Option<Arc<ProviderFactory>>,
    turn: TurnId,
    mut request: crate::domain::ChatRequest,
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

    let max_context_tokens = provider.capabilities().max_context_tokens.or_else(|| {
        crate::domain::runtime::infer_static_context_window_for_model_id(&request.model_id)
    });
    let context_snapshot =
        crate::domain::estimate_context_usage_for_request(&request, max_context_tokens);
    let _ = msg_tx
        .send(Msg::ContextUsageEstimated {
            turn,
            snapshot: context_snapshot.clone(),
        })
        .await;

    let policy = CompactionPolicy::default();
    let mut compacted_before_stream = false;
    if crate::domain::should_auto_compact(&context_snapshot, &request, policy).is_ok() {
        let compaction = CompactionRequest::auto(request.clone(), CompactionTrigger::AutoThreshold);
        match run_compaction(
            Arc::clone(&provider),
            turn,
            compaction,
            context_snapshot.clone(),
            max_context_tokens,
            token.clone(),
        )
        .await
        {
            Ok(result) => {
                request.messages = result.replacement_messages.clone();
                compacted_before_stream = true;
                let _ = msg_tx.send(Msg::CompactionFinished { turn, result }).await;
            },
            Err(err) => {
                let hard_limit =
                    crate::domain::context_exceeds_hard_limit(&context_snapshot, &request, policy);
                let _ = msg_tx
                    .send(Msg::CompactionFailed {
                        turn,
                        trigger: CompactionTrigger::AutoThreshold,
                        message: err.to_string(),
                        kind: if hard_limit {
                            crate::domain::StatusKind::Error
                        } else {
                            crate::domain::StatusKind::Warn
                        },
                    })
                    .await;
                if hard_limit {
                    let error = UserFacingError {
                        summary: "Context too large".to_string(),
                        message: format!(
                            "The next request needs {} tokens before response reserve, and automatic compaction failed: {}",
                            context_snapshot.used_tokens, err
                        ),
                        suggestion: "Run /compact with focus instructions, or /clear to start a fresh session.".to_string(),
                        category: crate::models::ErrorCategory::Config,
                        recoverable: true,
                    };
                    let _ = msg_tx.send(Msg::UpstreamError { turn, error }).await;
                    return;
                }
            },
        }
    }

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
    //
    // `ModelError::Cancelled` is swallowed — the terminal
    // `Msg::TurnCancelled` is emitted from `drop_scope` after the
    // turn's `TurnScope` drains. Emitting `UpstreamError` here would
    // commit a "cancelled" message the user didn't ask to see.
    match provider.chat(request.clone(), ctx).await {
        Ok(_final_response) => {
            // Success — the final `Done` flowed through the sink.
        },
        Err(crate::models::ModelError::Cancelled) => {
            // Silent: `drop_scope` will emit `Msg::TurnCancelled`.
        },
        Err(e) => {
            let retry_context_limit = !compacted_before_stream && is_context_limit_error(&e);
            if retry_context_limit {
                let latest_snapshot =
                    crate::domain::estimate_context_usage_for_request(&request, max_context_tokens);
                let compaction =
                    CompactionRequest::auto(request.clone(), CompactionTrigger::ContextLimitRetry);
                match run_compaction(
                    Arc::clone(&provider),
                    turn,
                    compaction,
                    latest_snapshot,
                    max_context_tokens,
                    token.clone(),
                )
                .await
                {
                    Ok(result) => {
                        let mut retry_request = request;
                        retry_request.messages = result.replacement_messages.clone();
                        let _ = msg_tx.send(Msg::CompactionFinished { turn, result }).await;
                        let _ = relay.await;
                        dispatch_provider_stream(msg_tx, provider, turn, retry_request, token)
                            .await;
                        return;
                    },
                    Err(compact_err) => {
                        let _ = msg_tx
                            .send(Msg::CompactionFailed {
                                turn,
                                trigger: CompactionTrigger::ContextLimitRetry,
                                message: compact_err.to_string(),
                                kind: crate::domain::StatusKind::Error,
                            })
                            .await;
                    },
                }
            }
            let error = classify_error_for_ui(&e);
            let _ = msg_tx.send(Msg::UpstreamError { turn, error }).await;
        },
    }

    let _ = relay.await;
}

async fn dispatch_provider_stream(
    msg_tx: MsgSender,
    provider: Arc<dyn ModelProvider>,
    turn: TurnId,
    request: crate::domain::ChatRequest,
    token: tokio_util::sync::CancellationToken,
) {
    let (stream_tx, mut stream_rx) = mpsc::channel::<StreamEvent>(256);
    let ctx = StreamContext::new(token.clone(), stream_tx, turn);
    let relay_tx = msg_tx.clone();
    let relay = tokio::spawn(async move {
        while let Some(event) = stream_rx.recv().await {
            let msg = match event {
                StreamEvent::Text(chunk) => Msg::StreamText { turn, chunk },
                StreamEvent::Reasoning(chunk) => Msg::StreamReasoning { turn, chunk },
                StreamEvent::ToolCall(call) => Msg::StreamToolCall { turn, call },
                StreamEvent::ThinkingSignature(_) => continue,
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

    match provider.chat(request, ctx).await {
        Ok(_) | Err(ModelError::Cancelled) => {},
        Err(e) => {
            let error = classify_error_for_ui(&e);
            let _ = msg_tx.send(Msg::UpstreamError { turn, error }).await;
        },
    }

    let _ = relay.await;
}

async fn dispatch_compact_conversation(
    msg_tx: MsgSender,
    providers: Option<Arc<ProviderFactory>>,
    turn: TurnId,
    request: CompactionRequest,
    token: tokio_util::sync::CancellationToken,
) {
    let Some(factory) = providers else {
        let _ = msg_tx
            .send(Msg::CompactionFailed {
                turn,
                trigger: request.trigger,
                message: "EffectRunner has no ProviderFactory bound".to_string(),
                kind: crate::domain::StatusKind::Error,
            })
            .await;
        return;
    };

    let provider = match factory.resolve(&request.chat.model_id).await {
        Ok(provider) => provider,
        Err(err) => {
            let _ = msg_tx
                .send(Msg::CompactionFailed {
                    turn,
                    trigger: request.trigger,
                    message: err.to_string(),
                    kind: crate::domain::StatusKind::Error,
                })
                .await;
            return;
        },
    };

    let max_context_tokens = provider.capabilities().max_context_tokens.or_else(|| {
        crate::domain::runtime::infer_static_context_window_for_model_id(&request.chat.model_id)
    });
    let before_snapshot =
        crate::domain::estimate_context_usage_for_request(&request.chat, max_context_tokens);

    let trigger = request.trigger;
    match run_compaction(
        provider,
        turn,
        request,
        before_snapshot,
        max_context_tokens,
        token,
    )
    .await
    {
        Ok(result) => {
            let _ = msg_tx.send(Msg::CompactionFinished { turn, result }).await;
        },
        Err(err) => {
            let _ = msg_tx
                .send(Msg::CompactionFailed {
                    turn,
                    trigger,
                    message: err.to_string(),
                    kind: crate::domain::StatusKind::Error,
                })
                .await;
        },
    }
}

async fn run_compaction(
    provider: Arc<dyn ModelProvider>,
    turn: TurnId,
    request: CompactionRequest,
    before_snapshot: crate::domain::ContextUsageSnapshot,
    max_context_tokens: Option<usize>,
    token: tokio_util::sync::CancellationToken,
) -> Result<CompactionResult, ModelError> {
    let started = Instant::now();
    let prepared = crate::domain::prepare_compaction(&request, max_context_tokens)
        .map_err(|skip| ModelError::InvalidRequest(skip.to_string()))?;

    let summary_request = crate::domain::build_summary_request(
        &request.chat,
        &prepared,
        request.instructions.as_deref(),
        request.policy,
    );
    let (draft, draft_usage) =
        collect_compaction_text(Arc::clone(&provider), turn, summary_request, token.clone())
            .await?;
    let draft_summary = crate::domain::normalize_summary(&draft);
    if draft_summary.trim().is_empty() {
        return Err(ModelError::InvalidRequest(
            "compaction produced an empty summary".to_string(),
        ));
    }

    let verify_request = crate::domain::build_verification_request(
        &request.chat,
        &prepared,
        &draft_summary,
        request.instructions.as_deref(),
        request.policy,
    );
    let (verified, verify_usage) =
        collect_compaction_text(Arc::clone(&provider), turn, verify_request, token).await?;
    let verified_summary = crate::domain::normalize_summary(&verified);
    let final_summary = if verified_summary.trim().is_empty() {
        draft_summary
    } else {
        verified_summary
    };

    let id = format!(
        "compact_{}",
        chrono::Local::now().format("%Y%m%d_%H%M%S_%3f")
    );
    let mut record = crate::domain::CompactionRecord {
        id,
        trigger: request.trigger,
        created_at: chrono::Local::now(),
        before_tokens: before_snapshot.used_tokens,
        after_tokens: 0,
        archived_message_count: prepared.archived_messages.len(),
        preserved_message_count: prepared.preserved_messages.len(),
        summary_tokens: final_summary.len().div_ceil(4),
        duration_secs: started.elapsed().as_secs_f64(),
        focus: request.instructions.clone(),
        archive_path: None,
    };

    let mut replacement =
        crate::domain::build_replacement_messages(&final_summary, &prepared, &record);
    let mut compacted_request = request.chat.clone();
    compacted_request.messages = replacement.clone();
    let mut after_snapshot =
        crate::domain::estimate_context_usage_for_request(&compacted_request, max_context_tokens);
    record.after_tokens = after_snapshot.used_tokens;
    record.duration_secs = started.elapsed().as_secs_f64();
    replacement = crate::domain::build_replacement_messages(&final_summary, &prepared, &record);
    compacted_request.messages = replacement.clone();
    after_snapshot =
        crate::domain::estimate_context_usage_for_request(&compacted_request, max_context_tokens);
    record.after_tokens = after_snapshot.used_tokens;

    if after_snapshot.used_tokens >= before_snapshot.used_tokens {
        return Err(ModelError::InvalidRequest(format!(
            "compaction did not reduce context ({} -> {} tokens)",
            before_snapshot.used_tokens, after_snapshot.used_tokens
        )));
    }

    if crate::domain::context_exceeds_hard_limit(
        &after_snapshot,
        &compacted_request,
        request.policy,
    ) {
        return Err(ModelError::InvalidRequest(format!(
            "compacted context still exceeds response reserve ({} tokens used)",
            after_snapshot.used_tokens
        )));
    }

    Ok(CompactionResult {
        record,
        replacement_messages: replacement,
        archived_messages: prepared.archived_messages,
        before_snapshot,
        after_snapshot,
        usage: crate::domain::combine_usage(draft_usage, verify_usage),
    })
}

async fn collect_compaction_text(
    provider: Arc<dyn ModelProvider>,
    turn: TurnId,
    request: crate::domain::ChatRequest,
    token: tokio_util::sync::CancellationToken,
) -> Result<(String, Option<TokenUsage>), ModelError> {
    let (stream_tx, mut stream_rx) = mpsc::channel::<StreamEvent>(128);
    let ctx = StreamContext::new(token, stream_tx, turn);
    let collector = tokio::spawn(async move {
        let mut text = String::new();
        let mut usage = None;
        while let Some(event) = stream_rx.recv().await {
            match event {
                StreamEvent::Text(chunk) => text.push_str(&chunk),
                StreamEvent::Done {
                    usage: done_usage, ..
                } => usage = done_usage,
                StreamEvent::Reasoning(_)
                | StreamEvent::ToolCall(_)
                | StreamEvent::ThinkingSignature(_) => {},
            }
        }
        (text, usage)
    });

    let response = provider.chat(request, ctx).await;
    let (text, stream_usage) = collector
        .await
        .map_err(|err| ModelError::StreamError(format!("compaction collector failed: {}", err)))?;
    match response {
        Ok(final_response) => Ok((text, final_response.usage.or(stream_usage))),
        Err(err) => Err(err),
    }
}

fn is_context_limit_error(error: &ModelError) -> bool {
    let text = error.to_string().to_lowercase();
    text.contains("context")
        && (text.contains("too large")
            || text.contains("exceed")
            || text.contains("maximum")
            || text.contains("token"))
}

/// Dispatch an `ExecuteTool` command.
#[allow(clippy::too_many_arguments)]
async fn dispatch_execute_tool(
    msg_tx: MsgSender,
    tools: Option<Arc<ToolRegistry>>,
    workdir: PathBuf,
    turn: TurnId,
    call_id: crate::domain::ToolCallId,
    source: crate::models::tool_call::ToolCall,
    token: tokio_util::sync::CancellationToken,
    config: Arc<crate::app::Config>,
    model_id: String,
) {
    let _ = msg_tx.send(Msg::ToolStarted { turn, call_id }).await;

    let Some(registry) = tools else {
        let _ = msg_tx
            .send(Msg::ToolFinished {
                turn,
                call_id,
                outcome: crate::domain::ToolOutcome::error(
                    "EffectRunner has no ToolRegistry bound",
                    0.0,
                ),
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
                    outcome: crate::domain::ToolOutcome::error(
                        format!("invalid MCP tool name: {}", source.function.name),
                        0.0,
                    ),
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
                outcome: crate::domain::ToolOutcome::error(
                    format!("unknown tool: {}", tool_key),
                    0.0,
                ),
            })
            .await;
        return;
    };

    // Bridge the tool's progress channel to `Msg::ToolProgress`.
    // A sibling task drains progress events while the tool runs.
    // The channel closes when `progress_tx` drops (when `ctx`
    // drops at the end of `tool.execute`), which terminates the
    // relay loop cleanly.
    let (progress_tx, mut progress_rx) = mpsc::channel(16);
    let relay_tx = msg_tx.clone();
    let progress_relay = tokio::spawn(async move {
        while let Some(event) = progress_rx.recv().await {
            if relay_tx
                .send(Msg::ToolProgress {
                    turn,
                    call_id,
                    event,
                })
                .await
                .is_err()
            {
                break;
            }
        }
    });

    let ctx = ExecContext::new(token, progress_tx, call_id, turn, workdir, config, model_id);
    let outcome = tool.execute(args, ctx).await;
    let _ = progress_relay.await;
    let _ = msg_tx
        .send(Msg::ToolFinished {
            turn,
            call_id,
            outcome,
        })
        .await;
}

/// Spawn `ollama pull <model>` and stream its stdout lines as
/// `Msg::ModelPullProgress` status updates. Emits a final
/// `Msg::ModelPullFinished` on successful exit; on failure, emits a
/// single `ModelPullProgress` with the error text.
async fn dispatch_pull_ollama_model(tx: MsgSender, model: String) {
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::process::Command;

    let mut cmd = Command::new("ollama");
    cmd.arg("pull")
        .arg(&model)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = tx
                .send(Msg::ModelPullProgress(format!(
                    "ollama pull failed to start: {}",
                    e
                )))
                .await;
            return;
        },
    };

    if let Some(stdout) = child.stdout.take() {
        let tx_inner = tx.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                let _ = tx_inner.send(Msg::ModelPullProgress(line)).await;
            }
        });
    }

    match child.wait().await {
        Ok(status) if status.success() => {
            let _ = tx.send(Msg::ModelPullFinished { model }).await;
        },
        Ok(status) => {
            let _ = tx
                .send(Msg::ModelPullProgress(format!(
                    "ollama pull exited with status {}",
                    status.code().unwrap_or(-1)
                )))
                .await;
        },
        Err(e) => {
            let _ = tx
                .send(Msg::ModelPullProgress(format!(
                    "ollama pull wait error: {}",
                    e
                )))
                .await;
        },
    }
}

fn mcp_startup_msg(name: &str, started: bool, tools: Vec<crate::domain::McpToolSpec>) -> Msg {
    if started {
        Msg::McpServerReady {
            name: name.to_string(),
            tools,
        }
    } else {
        Msg::McpServerErrored {
            name: name.to_string(),
            reason: "server failed to start or initialize".to_string(),
        }
    }
}

/// Read the system clipboard on a blocking thread and emit a `Msg`
/// back into the main loop. Image content wins when present; falls
/// back to text; empty or error surface as `Msg::TransientStatus` so
/// the user gets visible feedback (a silent no-op on Ctrl+V would be
/// confusing, especially on macOS where `osascript` can take ~300ms).
///
/// `tokio::task::spawn_blocking` is the right primitive: `clipboard::
/// has_image` / `read_image_bytes` / `read_text` shell out to xclip /
/// wl-paste / pngpaste / PowerShell, all of which block synchronously.
async fn dispatch_read_clipboard(tx: MsgSender) {
    use crate::domain::{Paste, StatusKind};

    enum Outcome {
        Image { bytes: Vec<u8>, format: String },
        Text(String),
        Empty,
        Error(String),
    }

    let outcome = tokio::task::spawn_blocking(|| {
        if crate::clipboard::has_image() {
            match crate::clipboard::read_image_bytes() {
                Ok((bytes, format)) => Outcome::Image { bytes, format },
                Err(e) => Outcome::Error(format!("Clipboard image read failed: {}", e)),
            }
        } else {
            match crate::clipboard::read_text() {
                Ok(t) if !t.is_empty() => Outcome::Text(t),
                Ok(_) => Outcome::Empty,
                Err(e) => Outcome::Error(format!("Clipboard empty / read failed: {}", e)),
            }
        }
    })
    .await
    .unwrap_or_else(|e| Outcome::Error(format!("clipboard spawn_blocking: {}", e)));

    let msg = match outcome {
        Outcome::Image { bytes, format } => Msg::Paste(Paste::Image { bytes, format }),
        Outcome::Text(text) => Msg::Paste(Paste::Text(text)),
        Outcome::Empty => Msg::TransientStatus {
            text: "Clipboard is empty".to_string(),
            kind: StatusKind::Info,
            dismiss_ms: 2_000,
        },
        Outcome::Error(text) => Msg::TransientStatus {
            text,
            kind: StatusKind::Warn,
            dismiss_ms: 4_000,
        },
    };
    let _ = tx.send(msg).await;
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
    use crate::domain::ToolCallId;
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

    #[test]
    fn mcp_startup_msg_treats_zero_tool_started_server_as_ready() {
        let msg = mcp_startup_msg("empty", true, Vec::new());
        assert!(matches!(
            msg,
            Msg::McpServerReady { name, tools } if name == "empty" && tools.is_empty()
        ));
    }

    #[test]
    fn mcp_startup_msg_reports_unstarted_server_as_error() {
        let msg = mcp_startup_msg("bad", false, Vec::new());
        assert!(matches!(
            msg,
            Msg::McpServerErrored { name, reason }
                if name == "bad" && reason.contains("failed to start")
        ));
    }

    #[tokio::test]
    async fn cancel_scope_emits_turn_cancelled_after_bounded_timeout() {
        let (mut r, mut rx) = runner();
        let turn = TurnId(77);
        {
            let scope = r.scope_mut(turn);
            scope.spawn(async {
                std::future::pending::<()>().await;
            });
        }
        assert_eq!(r.scope_count(), 1);

        let start = std::time::Instant::now();
        r.dispatch(Cmd::CancelScope(turn));
        assert_eq!(r.scope_count(), 0);
        let msg = tokio::time::timeout(Duration::from_millis(500), rx.recv())
            .await
            .expect("bounded cancel should emit terminal message")
            .expect("channel alive");
        assert!(matches!(msg, Msg::TurnCancelled(t) if t == turn));
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "cancel terminal message took {:?}",
            start.elapsed()
        );
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

    /// F12: after a spawned task completes (here via the
    /// no-ProviderFactory error path), the next `dispatch` call reaps
    /// the empty scope instead of leaving an orphan entry in the map.
    #[tokio::test]
    async fn empty_scopes_are_reaped_on_next_dispatch() {
        let (mut r, mut rx) = runner();
        let turn = TurnId(42);
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

        // Runner has no provider bindings → dispatch_call_model hits
        // the "not wired" error path and emits UpstreamError, then the
        // spawned task returns. Drain that message so we know the task
        // ran to completion.
        let msg = tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .expect("upstream error arrived")
            .expect("channel alive");
        assert!(matches!(msg, Msg::UpstreamError { .. }));

        // Give the JoinSet a tick to notice the task finished.
        tokio::task::yield_now().await;

        // Any subsequent dispatch reaps the now-empty scope.
        r.dispatch(Cmd::DismissStatusAfter { ms: 10 });
        assert_eq!(
            r.scope_count(),
            0,
            "completed scope must be reaped on next dispatch"
        );
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
            model_id: "ollama/test".to_string(),
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
