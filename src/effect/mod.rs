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
//! `PersistReasoningFor`), MCP lifecycle
//! (`InitMcpServers`, `StopMcpServer`), local side-effects
//! (`WriteImageToTemp`, `OpenInSystem`, `PullOllamaModel`,
//! `SetTerminalTitle`). Cancellation flows
//! through `Cmd::CancelScope(TurnId)` → the scope's
//! `CancellationToken`.

mod config_watch;
mod middleware;
mod turn_scope;

use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Instant;

use tokio::sync::mpsc;

use crate::app::{Config, MemoryConfig};
use crate::domain::{
    Cmd, CompactionPolicy, CompactionRequest, CompactionResult, CompactionTrigger, Msg, TurnId,
};
use crate::models::{ModelError, TokenUsage};
use crate::providers::ctx::{ExecContext, StreamContext};
use crate::providers::model::ModelProvider;
use crate::providers::{ProviderFactory, StreamEvent, ToolRegistry};
use crate::utils::{join_logged, spawn_guarded};

pub use middleware::{DEFAULT_MAX_ATTEMPTS, retry_transient_http};
pub use turn_scope::TurnScope;

#[cfg(not(test))]
const CANCEL_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
#[cfg(test)]
const CANCEL_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(50);

/// F38: how many recently-cancelled `TurnId`s to remember as tombstones.
/// Turn ids are strictly monotonic and never reused, so a stray turn-scoped
/// `Cmd` for a cancelled turn can only ever be a post-cancel straggler that
/// lands within a few turns of the cancel. A small bounded ring is plenty;
/// older entries age out so the set never grows across a long session.
const CANCELLED_TOMBSTONE_CAP: usize = 256;

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

#[derive(Clone)]
enum PersistenceJob {
    Conversation(Box<crate::session::ConversationHistory>),
    Compaction(Box<PendingCompactionSave>),
}

#[derive(Clone)]
struct PendingCompactionSave {
    archive: crate::domain::CompactionArchive,
    record: crate::domain::CompactionRecord,
    conversation: crate::session::ConversationHistory,
    task_id: Option<String>,
}

struct PersistedCompaction {
    id: String,
    task_id: Option<String>,
    session_id: String,
    archive_path: PathBuf,
}

struct PersistenceState {
    workdir: PathBuf,
    manager: Option<crate::session::ConversationManager>,
    blocked: HashMap<String, VecDeque<PendingCompactionSave>>,
}

impl PersistenceState {
    fn new(workdir: PathBuf) -> Self {
        Self {
            workdir,
            manager: None,
            blocked: HashMap::new(),
        }
    }

    fn manager(&mut self) -> anyhow::Result<&crate::session::ConversationManager> {
        if self.manager.is_none() {
            self.manager = Some(crate::session::ConversationManager::new(&self.workdir)?);
        }
        Ok(self.manager.as_ref().expect("manager initialized"))
    }

    /// Run one job. Returns every compaction event that persisted durably —
    /// even when the job as a whole failed — so partially-drained barriers
    /// still fire their hooks and `SessionSaved`; a dropped event would never
    /// be re-emitted (its save is already popped).
    fn process(&mut self, job: PersistenceJob) -> (Vec<PersistedCompaction>, anyhow::Result<()>) {
        match job {
            PersistenceJob::Conversation(history) => {
                // Barrier: a still-blocked compaction must persist before any
                // newer (stripped) conversation snapshot may overwrite the file.
                let (persisted, retried) = self.retry_blocked(&history.id);
                if retried.is_err() {
                    return (persisted, retried);
                }
                let saved = self
                    .manager()
                    .and_then(|manager| manager.save_conversation(&history).map(|_| ()));
                (persisted, saved)
            },
            PersistenceJob::Compaction(save) => {
                // Queue first, then drain. The archive is the only durable copy
                // of the stripped messages, so the save must survive an Err AND
                // a panic in the write path (pop happens only after success),
                // and it must land behind any older still-blocked saves (FIFO).
                let conversation_id = save.archive.conversation_id.clone();
                self.blocked
                    .entry(conversation_id.clone())
                    .or_default()
                    .push_back(*save);
                self.retry_blocked(&conversation_id)
            },
        }
    }

    fn retry_blocked(
        &mut self,
        conversation_id: &str,
    ) -> (Vec<PersistedCompaction>, anyhow::Result<()>) {
        let mut persisted = Vec::new();
        if !self.blocked.contains_key(conversation_id) {
            return (persisted, Ok(()));
        }
        if let Err(error) = self.manager() {
            return (persisted, Err(error));
        }
        // Disjoint field borrows: the manager stays immutably borrowed while
        // the queue is drained in place — no per-retry clone of the (large)
        // pending conversation snapshots.
        let manager = self.manager.as_ref().expect("manager initialized");
        let queue = self
            .blocked
            .get_mut(conversation_id)
            .expect("checked above");
        while let Some(save) = queue.front() {
            match Self::persist_compaction(manager, save) {
                // Pop only after a successful write: `persist_compaction` runs
                // inside `spawn_blocking`, and a panic there must not lose the
                // save (the mutex is poison-tolerant, so the state survives).
                Ok(event) => {
                    persisted.push(event);
                    queue.pop_front();
                },
                Err(error) => return (persisted, Err(error)),
            }
        }
        self.blocked.remove(conversation_id);
        (persisted, Ok(()))
    }

    fn retry_all_blocked(&mut self) -> (Vec<PersistedCompaction>, anyhow::Result<()>) {
        let ids: Vec<String> = self.blocked.keys().cloned().collect();
        let mut persisted = Vec::new();
        let mut first_error = None;
        for id in ids {
            // Keep draining the other conversations' barriers; one
            // conversation's bad disk state must not strand the rest.
            let (events, result) = self.retry_blocked(&id);
            persisted.extend(events);
            if let Err(error) = result {
                first_error.get_or_insert(error);
            }
        }
        match first_error {
            None => (persisted, Ok(())),
            Some(error) => (persisted, Err(error)),
        }
    }

    fn persist_compaction(
        manager: &crate::session::ConversationManager,
        save: &PendingCompactionSave,
    ) -> anyhow::Result<PersistedCompaction> {
        let path = manager.save_compaction_archive(&save.archive)?;
        manager.save_conversation(&save.conversation)?;

        if let Ok(store) = crate::runtime::RuntimeStore::open_default() {
            let _ = store.compactions().create(crate::runtime::NewCompaction {
                id: Some(save.record.id.clone()),
                task_id: save.task_id.clone(),
                session_id: Some(save.archive.conversation_id.clone()),
                source_token_estimate: Some(save.record.before_tokens as i64),
                summary_token_count: Some(save.record.summary_tokens as i64),
                preserved_turns: Some(save.record.preserved_turn_count as i64),
                archive_path: Some(path.display().to_string()),
                verification_status: Some(save.record.review_status.as_str().to_string()),
            });
        }

        Ok(PersistedCompaction {
            id: save.record.id.clone(),
            task_id: save.task_id.clone(),
            session_id: save.archive.conversation_id.clone(),
            archive_path: path,
        })
    }
}

/// Fire the plugin `compaction` hook for one durably persisted archive.
async fn fire_compaction_hook(event: &PersistedCompaction) {
    fire_plugin_hooks(
        "compaction",
        serde_json::json!({
            "id": event.id,
            "task_id": event.task_id,
            "session_id": event.session_id,
            "archive_path": event.archive_path.display().to_string(),
        }),
    )
    .await;
}

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
    /// F38: bounded tombstone ring of `TurnId`s whose scope has been
    /// cancelled+dropped. A turn-scoped `Cmd` (`CallModel` / `ExecuteTool` /
    /// `CompactConversation`) bearing a tombstoned id is dropped in `dispatch`
    /// instead of resurrecting a fresh, un-cancelled scope through
    /// `scope_mut`'s `or_insert_with`. Bounded to `CANCELLED_TOMBSTONE_CAP`.
    cancelled_turns: VecDeque<TurnId>,
    /// Detached work (saves, persists, MCP lifecycle) lives here.
    /// This one set never gets cancelled piecemeal — shutdown drains
    /// it during `EffectRunner::shutdown`.
    detached: tokio::task::JoinSet<()>,
    /// FIFO chain for conversation and compaction writes. Keeping persistence
    /// separate from `detached` prevents an older compaction snapshot from
    /// racing a newer normal save and winning last-write-wins.
    persistence_state: Arc<Mutex<PersistenceState>>,
    persistence_tail: Option<tokio::task::JoinHandle<()>>,
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
    /// Durable runtime task that owns work launched by this runner.
    task_id: Option<String>,
    /// Interactive TUI runners write OSC 2 terminal-title updates.
    /// Headless `mermaid run` must suppress them so stdout stays
    /// machine-readable for JSON/markdown/text output modes.
    terminal_title_enabled: bool,
    /// Whether this runner's `shutdown` reaps the PROCESS-GLOBAL MCP manager
    /// (`crate::mcp::manager_ref`). True only for the top-level runner. A
    /// subagent's child runner shares the global manager, so it must NOT reap
    /// it — otherwise the first subagent to finish would kill every MCP
    /// server out from under the parent for the rest of the session.
    owns_global_mcp: bool,
    /// Inline-approval broker. `Some` only for interactive TUI runs (set via
    /// `with_interactive_approvals`); headless + child runners leave it `None`,
    /// so the gate falls back to the out-of-band DB-approval flow.
    approval: Option<crate::providers::ApprovalBroker>,
    /// Inline-question broker for `ask_user_question`. `Some` only for
    /// interactive TUI runs (set via `with_interactive_questions`); headless +
    /// child runners leave it `None`, so the tool proceeds without asking.
    questions: Option<crate::providers::QuestionBroker>,
    /// Checklist broker for the task tools. Built unconditionally — unlike
    /// `questions`, task tracking works headless, and a subagent's child
    /// runner minting its own broker (bound to the CHILD's msg channel) is
    /// exactly what isolates its checklist from the parent's.
    tasks: crate::providers::TaskBroker,
    /// Abort handle for the background config watcher (#45). It's a perpetual
    /// loop living in `detached`, so `shutdown` aborts it explicitly before
    /// draining — otherwise the drain would block on it until the timeout.
    config_watch: Option<tokio::task::AbortHandle>,
}

impl EffectRunner {
    /// Create an unused runner. Pair with `msg_rx` from `channel()`.
    pub fn new(msg_tx: MsgSender, workdir: PathBuf) -> Self {
        let persistence_state = Arc::new(Mutex::new(PersistenceState::new(workdir.clone())));
        Self {
            tasks: crate::providers::TaskBroker::new(msg_tx.clone()),
            msg_tx,
            scopes: HashMap::new(),
            cancelled_turns: VecDeque::new(),
            detached: tokio::task::JoinSet::new(),
            persistence_state,
            persistence_tail: None,
            workdir,
            providers: None,
            tools: None,
            task_id: None,
            terminal_title_enabled: true,
            owns_global_mcp: true,
            approval: None,
            questions: None,
            config_watch: None,
        }
    }

    /// Enable inline approval prompts (interactive TUI only). The gate then
    /// pauses gated tools and routes the user's decision through the
    /// `ApprovalBroker` instead of writing an out-of-band DB approval row.
    pub fn with_interactive_approvals(mut self) -> Self {
        self.approval = Some(crate::providers::ApprovalBroker::new(self.msg_tx.clone()));
        self
    }

    /// Enable inline `ask_user_question` prompts (interactive TUI only). The tool
    /// then parks on the `QuestionBroker` and routes the user's answers back
    /// through it instead of proceeding without asking.
    pub fn with_interactive_questions(mut self) -> Self {
        self.questions = Some(crate::providers::QuestionBroker::new(self.msg_tx.clone()));
        self
    }

    /// Start the background config watcher (#45): it polls `MERMAID.md` + memory
    /// and emits `Msg::InstructionsChanged`/`MemoryChanged` on change, so the
    /// reducer reads them as injected data instead of refreshing inline. Call
    /// once at startup. Live-loop only — a replay driver feeds the recorded
    /// Changed Msgs rather than polling.
    pub fn spawn_config_watcher(&mut self, cwd: PathBuf, memory: MemoryConfig) {
        let handle = self.detached.spawn(config_watch::config_watcher(
            self.msg_tx.clone(),
            cwd,
            memory,
        ));
        self.config_watch = Some(handle);
    }

    /// Attach a durable runtime task id so tool runs, approvals,
    /// checkpoints, compactions, and background processes can be linked.
    pub fn with_task_id(mut self, task_id: Option<String>) -> Self {
        self.task_id = task_id;
        self
    }

    /// Disable terminal-title writes for non-interactive callers.
    pub fn without_terminal_title(mut self) -> Self {
        self.terminal_title_enabled = false;
        self
    }

    /// Leave the process-global MCP manager alone on `shutdown`. Child
    /// (subagent) runners share it with the parent and must not reap it.
    pub fn without_global_mcp_shutdown(mut self) -> Self {
        self.owns_global_mcp = false;
        self
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

    pub fn pair_from_with_task(
        workdir: PathBuf,
        providers: Arc<ProviderFactory>,
        tools: Arc<ToolRegistry>,
        task_id: Option<String>,
    ) -> (Self, mpsc::Receiver<Msg>) {
        let (runner, rx) = Self::pair_from(workdir, providers, tools);
        (runner.with_task_id(task_id), rx)
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
        // A subagent's runner is never the interactive top-level, so it must
        // NOT emit OSC 2 terminal-title escapes: in a headless `mermaid run`
        // the parent suppresses them, but an un-suppressed child leaks
        // `\x1b]2;…\x07` into stdout and corrupts `--format json`/`text` output.
        // It must also leave the process-global MCP manager running — the
        // child shares the parent's servers, and reaping them here would kill
        // MCP for the whole session the moment the first subagent finished.
        Self::new(msg_tx, workdir)
            .with_bindings(providers, tools)
            .without_terminal_title()
            .without_global_mcp_shutdown()
    }

    /// Get or create the scope for a turn. Idempotent. The scope is
    /// retained until `CancelScope` tears it down or it naturally
    /// drains.
    fn scope_mut(&mut self, turn: TurnId) -> &mut TurnScope {
        self.scopes
            .entry(turn)
            .or_insert_with(|| TurnScope::new(turn))
    }

    /// F38: record a cancelled turn in the bounded tombstone ring, evicting the
    /// oldest id at capacity. Skips duplicates so a re-cancel doesn't churn the
    /// ring (membership is all `is_tombstoned` checks).
    fn tombstone_turn(&mut self, turn: TurnId) {
        if self.cancelled_turns.contains(&turn) {
            return;
        }
        if self.cancelled_turns.len() >= CANCELLED_TOMBSTONE_CAP {
            self.cancelled_turns.pop_front();
        }
        self.cancelled_turns.push_back(turn);
    }

    /// F38: true iff `turn`'s scope was cancelled (tombstoned). New turn-scoped
    /// work for such a turn is dropped rather than spinning up a fresh scope.
    fn is_tombstoned(&self, turn: TurnId) -> bool {
        self.cancelled_turns.contains(&turn)
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
        // F38: tombstone this turn so a stray post-cancel turn-scoped Cmd can't
        // resurrect an un-cancelled scope for it. Recorded for both the live and
        // already-reaped branches below — once cancelled, a turn is dead either
        // way (turn ids are monotonic and never reused).
        self.tombstone_turn(turn);
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
        } else {
            // The scope was already reaped — its `JoinSet` drained to empty
            // and `reap_empty_scopes` (top of `dispatch`) removed it before
            // this cancel landed. The reducer is still in `Cancelling` with
            // no other way to learn the turn ended, so emit the terminal
            // event anyway. Idempotent: `handle_turn_cancelled` no-ops on
            // any turn that isn't currently `Cancelling`.
            let tx = self.msg_tx.clone();
            self.detached.spawn(async move {
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
        self.reap_detached();
        self.scopes.retain(|_, scope| {
            scope.drain_completed();
            !scope.is_empty()
        });
    }

    /// Harvest finished detached tasks. Without this the `detached` JoinSet
    /// grows for the whole session (every fire-and-forget effect lingers as a
    /// completed-but-unjoined handle), and a panicking detached task vanishes
    /// without a trace. Non-blocking — only already-finished tasks are taken (#38).
    fn reap_detached(&mut self) {
        while let Some(result) = self.detached.try_join_next() {
            if let Err(e) = result
                && !e.is_cancelled()
            {
                tracing::warn!(error = %e, "effect: detached task panicked");
            }
        }
    }

    /// Route a single `Cmd` into the appropriate spawn + handler.
    /// Returns immediately; handlers work asynchronously and emit
    /// `Msg` back through the sender channel.
    pub fn dispatch(&mut self, cmd: Cmd) {
        // F12: reap any drained scopes before touching the map. Keeps
        // `scope_count()` bounded as the session grows.
        self.reap_empty_scopes();
        tracing::trace!(cmd = %cmd.summary(), "effect: dispatch");

        // F38: refuse to spawn fresh work for a turn we've already cancelled.
        // Only the scope-spawning variants carry a `scope_turn()`; `CancelScope`
        // returns `None` here so a re-cancel still reaches `drop_scope` (which
        // re-emits the terminal `TurnCancelled` the reducer needs). Turn ids are
        // monotonic and never reused, so a tombstoned id can only be a stray
        // post-cancel straggler — dropping it stops `scope_mut`'s `or_insert_with`
        // from resurrecting an un-cancelled scope.
        if let Some(turn) = cmd.scope_turn()
            && self.is_tombstoned(turn)
        {
            tracing::debug!(
                cmd = %cmd.summary(),
                turn = %turn,
                "effect: dropping turn-scoped cmd for an already-cancelled turn"
            );
            return;
        }

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
                // Formatting turns (`output_schema`) advertise NO tools —
                // the reducer already sent none; don't re-add built-ins.
                if let Some(tools) = &self.tools
                    && request.output_schema.is_none()
                {
                    let mut enriched = tools.describe_all();
                    // Report the built-in tool-schema token cost so the
                    // reducer's /context preview can fold it into its MCP-only
                    // estimate and agree with what the model actually sees.
                    let builtin_tokens = crate::domain::estimate_tool_schema_tokens(&enriched);
                    // Best-effort and cosmetic (the /context preview). This is the
                    // synchronous dispatch path so we can't await; if the bounded
                    // channel is momentarily full under heavy streaming, log the
                    // drop rather than swallowing it silently — the estimate just
                    // stays briefly stale (#F43).
                    if let Err(e) = tx.try_send(Msg::BuiltinToolSchemaTokens(builtin_tokens)) {
                        tracing::debug!(
                            error = %e,
                            "effect: dropped builtin tool-schema token estimate (channel full); \
                             /context preview may be briefly stale"
                        );
                    }
                    enriched.append(&mut request.tools);
                    request.tools = enriched;
                }
                // Detached + off the blocking pool: never run a plugin hook on
                // the synchronous dispatch path (it would freeze input/render).
                self.detached.spawn(fire_plugin_hooks(
                    "prompt_submit",
                    serde_json::json!({
                        "turn_id": turn.0,
                        "model_id": request.model_id.clone(),
                        "message_count": request.messages.len(),
                        "tool_count": request.tools.len(),
                    }),
                ));
                // Task cost attribution: model dispatch reports each request's
                // completion tokens into the broker's cumulative counter.
                let task_usage = self.tasks.clone();
                let scope = self.scope_mut(turn);
                let token = scope.token();
                scope.spawn(async move {
                    use futures::FutureExt;
                    let fallback_tx = tx.clone();
                    if std::panic::AssertUnwindSafe(dispatch_call_model(
                        tx, providers, turn, request, token, task_usage,
                    ))
                    .catch_unwind()
                    .await
                    .is_err()
                    {
                        // The dispatch task panicked. A turn whose model call
                        // never emits a terminal Msg stays in `Generating`
                        // forever; emit one so the reducer can leave that state
                        // instead of wedging (#43).
                        tracing::error!(turn = %turn, "dispatch_call_model panicked");
                        let _ = fallback_tx
                            .send(Msg::UpstreamError {
                                turn,
                                error: crate::models::UserFacingError {
                                    summary: "Internal error".to_string(),
                                    message: "The model dispatch task panicked unexpectedly."
                                        .to_string(),
                                    suggestion: "This is a bug. Please retry; if it persists, \
                                                 check the logs."
                                        .to_string(),
                                    category: crate::models::ErrorCategory::Internal,
                                    recoverable: true,
                                },
                            })
                            .await;
                    }
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
                // Capture the trigger before `request` moves into the task, so a
                // panic fallback can still name which compaction failed.
                let trigger = request.trigger;
                let scope = self.scope_mut(turn);
                let token = scope.token();
                scope.spawn(async move {
                    use futures::FutureExt;
                    let fallback_tx = tx.clone();
                    if std::panic::AssertUnwindSafe(dispatch_compact_conversation(
                        tx, providers, turn, request, token,
                    ))
                    .catch_unwind()
                    .await
                    .is_err()
                    {
                        // The compaction task panicked. Without a terminal
                        // `CompactionFinished`/`CompactionFailed`, the reducer
                        // wedges in `Compacting` until Ctrl+C; emit a failure so
                        // it can recover, mirroring `CallModel`/`ExecuteTool`
                        // (#43, F37).
                        tracing::error!(turn = %turn, "dispatch_compact_conversation panicked");
                        let _ = fallback_tx
                            .send(Msg::CompactionFailed {
                                turn,
                                trigger,
                                message: "the compaction task panicked unexpectedly".to_string(),
                                kind: crate::domain::StatusKind::Error,
                            })
                            .await;
                    }
                });
            },
            Cmd::ExecuteTool {
                turn,
                call_id,
                source,
                model_id,
                safety_mode,
                plan_file,
                plan_permissions,
                context_percent,
                intent,
                session_id,
                message_index,
                scratchpad,
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
                // Auto mode: build an LLM classifier to vet borderline
                // actions. Only when a provider is bound (real wiring); the
                // gate fails safe to "escalate" when it's `None`. The vet
                // uses the configured classifier model, else the session model.
                // Plan mode also gets one: profile levels set to `auto`
                // resolve through `PolicyDecision::Classify`, which fails
                // safe to escalate without a classifier bound.
                let classifier: Option<Arc<dyn crate::providers::AutoClassifier>> =
                    if safety_mode == crate::runtime::SafetyMode::Auto || plan_file.is_some() {
                        self.providers.as_ref().map(|p| {
                            let model = config
                                .safety
                                .auto_classifier_model
                                .clone()
                                .unwrap_or_else(|| model_id.clone());
                            Arc::new(crate::providers::ModelAutoClassifier::new(p.clone(), model))
                                as Arc<dyn crate::providers::AutoClassifier>
                        })
                    } else {
                        None
                    };
                let task_id = self.task_id.clone();
                let approval = self.approval.clone();
                let questions = self.questions.clone();
                let task_broker = self.tasks.clone();
                let scope = self.scope_mut(turn);
                let token = scope.token();
                let background = scope.background_token();
                scope.spawn(async move {
                    use futures::FutureExt;
                    let fallback_tx = tx.clone();
                    if std::panic::AssertUnwindSafe(dispatch_execute_tool(
                        tx,
                        tools,
                        workdir,
                        turn,
                        call_id,
                        source,
                        token,
                        background,
                        config,
                        model_id,
                        task_id,
                        session_id,
                        message_index,
                        scratchpad,
                        safety_mode,
                        plan_file,
                        plan_permissions,
                        context_percent,
                        intent,
                        classifier,
                        approval,
                        questions,
                        task_broker,
                    ))
                    .catch_unwind()
                    .await
                    .is_err()
                    {
                        // The tool task panicked. Its turn waits on a
                        // `ToolFinished` for this `call_id` that will now never
                        // arrive; emit a terminal error outcome so the turn
                        // doesn't wedge (#43).
                        tracing::error!(
                            turn = %turn,
                            call_id = call_id.0,
                            "dispatch_execute_tool panicked"
                        );
                        let _ = fallback_tx
                            .send(Msg::ToolFinished {
                                turn,
                                call_id,
                                outcome: crate::domain::ToolOutcome::error(
                                    "internal error: the tool execution task panicked".to_string(),
                                    0.0,
                                ),
                            })
                            .await;
                    }
                });
            },
            Cmd::ResolveApproval { call_id, decision } => {
                // Deliver the user's inline decision to the parked tool task.
                // Not turn-scoped — fire-and-forget to the broker.
                if let Some(broker) = &self.approval {
                    broker.resolve(call_id, decision.into());
                }
            },
            Cmd::ResolveQuestion {
                call_id,
                resolution,
            } => {
                // Deliver the user's answers to the parked ask_user_question
                // task. Not turn-scoped — fire-and-forget to the broker.
                if let Some(broker) = &self.questions {
                    broker.resolve(call_id, resolution);
                }
            },
            Cmd::SyncTaskStore(store) => {
                // Reducer-initiated truth overwrite (rewind/fork, /clear,
                // startup resume). Synchronous; the broker does not publish
                // back — the reducer already holds this store.
                self.tasks.seed(store);
            },
            Cmd::EnsureScratchpad { session_id } => {
                let tx = self.msg_tx.clone();
                let workdir = self.workdir.clone();
                self.detached.spawn(async move {
                    match crate::session::scratchpad::ensure(&workdir, &session_id) {
                        Ok(path) => {
                            let _ = tx.send(Msg::ScratchpadReady { session_id, path }).await;
                        },
                        Err(err) => {
                            // Non-fatal: the session runs without a scratch
                            // dir (`Session::scratchpad` stays `None`).
                            tracing::warn!(error = %err, "failed to create session scratchpad");
                        },
                    }
                    // Best-effort reap of unlocked scratchpads past retention —
                    // piggybacks on session startup, no separate timer.
                    if let Err(err) = crate::session::scratchpad::sweep_stale(
                        crate::session::scratchpad::RETENTION_DAYS,
                    ) {
                        tracing::warn!(error = %err, "scratchpad sweep failed");
                    }
                });
            },
            Cmd::ListScratchpad { path } => {
                // `/scratchpad` — bounded directory listing back into the
                // transcript. Blocking filesystem walk, so off the runner.
                let tx = self.msg_tx.clone();
                self.detached.spawn(async move {
                    let text = tokio::task::spawn_blocking(move || {
                        crate::session::scratchpad::list_text(&path)
                    })
                    .await
                    .unwrap_or_else(|e| format!("Couldn't list the scratchpad: {e}"));
                    let _ = tx.send(Msg::RuntimeText(text)).await;
                });
            },
            Cmd::UserTaskEdit(edit) => {
                // Route the user's /tasks edit through the broker (single
                // writer) so it serializes with any in-flight tool call. The
                // broker publishes the resulting snapshot; the outcome line
                // lands in the transcript as transient status.
                let broker = self.tasks.clone();
                let tx = self.msg_tx.clone();
                self.detached.spawn(async move {
                    let (line, _snapshot) = broker.user_edit(edit).await;
                    // The user sees the ack in the transcript; the model
                    // learns about it on its next request via the notice
                    // buffer (a checklist the model believes in but the user
                    // has edited is the worst of both).
                    let _ = tx
                        .send(Msg::TaskNotice {
                            text: format!(
                                "The user edited the task checklist: {line}. Acknowledge and \
                                 incorporate this into your plan."
                            ),
                        })
                        .await;
                    let _ = tx.send(Msg::TransientStatus { text: line }).await;
                });
            },
            Cmd::NotifyTaskCompleted {
                task,
                completed,
                total,
            } => {
                // Gated `task_completed` plugin hook: a denying hook VETOES
                // the completion — the task flips back to in_progress via the
                // broker (single writer; the publish refreshes the band) and
                // the reason reaches both the user (transcript) and the model
                // (notice buffer). Fail-open like every plugin hook: no
                // enabled hooks / timeout => allow, zero latency added
                // elsewhere because this runs detached.
                let payload = serde_json::json!({
                    "task_id": task.id,
                    "subject": task.subject,
                    "description": task.description,
                    "evidence": task.evidence,
                    "completed": completed,
                    "total": total,
                });
                let broker = self.tasks.clone();
                let tx = self.msg_tx.clone();
                self.detached.spawn(async move {
                    let gate = run_plugin_hooks_gated("task_completed", payload).await;
                    let Some((plugin, reason)) = gate.deny else {
                        return;
                    };
                    let reason = crate::utils::redact_secrets(&reason);
                    let _ = broker
                        .update(vec![crate::domain::TaskEdit {
                            id: task.id,
                            status: Some(crate::domain::TaskStatus::InProgress),
                            ..crate::domain::TaskEdit::default()
                        }])
                        .await;
                    let _ = tx
                        .send(Msg::TaskNotice {
                            text: format!(
                                "Completion of task #{} '{}' was vetoed by the {plugin} hook: \
                                 {reason}. The task is back in_progress; address the reason \
                                 before completing it again.",
                                task.id, task.subject
                            ),
                        })
                        .await;
                    let _ = tx
                        .send(Msg::TransientStatus {
                            text: format!(
                                "task #{} completion vetoed by {plugin}: {reason}",
                                task.id
                            ),
                        })
                        .await;
                });
            },
            Cmd::CancelScope(turn) => {
                self.drop_scope(turn);
            },
            Cmd::BackgroundScope(turn) => {
                // Fire the scope's background token (don't drop the scope):
                // detachable tools move their child to a background process and
                // return a normal outcome, so the turn finishes naturally.
                self.scope_mut(turn).background();
            },
            Cmd::SaveConversation(history) => {
                self.queue_persistence(PersistenceJob::Conversation(Box::new(history)));
            },
            Cmd::SaveCompactionArchive {
                archive,
                record,
                conversation,
            } => {
                self.queue_persistence(PersistenceJob::Compaction(Box::new(
                    PendingCompactionSave {
                        archive,
                        record,
                        conversation,
                        task_id: self.task_id.clone(),
                    },
                )));
            },
            Cmd::SaveProcess(process) => {
                let task_id = self.task_id.clone();
                self.detached.spawn(async move {
                    let status = match process.status {
                        crate::domain::ManagedProcessStatus::Running => {
                            crate::runtime::ProcessStatus::Running
                        },
                        crate::domain::ManagedProcessStatus::Exited => {
                            crate::runtime::ProcessStatus::Exited
                        },
                        crate::domain::ManagedProcessStatus::Unknown => {
                            crate::runtime::ProcessStatus::Unknown
                        },
                    };
                    if let Ok(store) = crate::runtime::RuntimeStore::open_default() {
                        let _ = store.processes().upsert(crate::runtime::NewProcess {
                            id: Some(process.id),
                            task_id,
                            pid: process.pid,
                            command: process.command,
                            cwd: process.cwd,
                            log_path: Some(process.log_path),
                            detected_url: process.detected_url,
                            status,
                            health: None,
                        });
                    }
                });
            },
            Cmd::PersistPlanConfig(plan) => {
                self.detached.spawn(async move {
                    if let Err(err) = crate::app::persist_plan_config(&plan) {
                        tracing::warn!(error = %err, "failed to persist [plan] config");
                    }
                });
            },
            Cmd::PersistLastModel(model) => {
                self.detached.spawn(async move {
                    if let Err(err) = crate::app::persist_last_model(&model) {
                        tracing::warn!(error = %err, "failed to persist last-used model");
                    }
                });
            },
            Cmd::PersistReasoningFor { model_id, level } => {
                self.detached.spawn(async move {
                    if let Err(err) = crate::app::persist_reasoning_for_model(&model_id, level) {
                        tracing::warn!(error = %err, "failed to persist reasoning level for model");
                    }
                });
            },
            Cmd::PersistOllamaNumCtxFor { model_id, num_ctx } => {
                self.detached.spawn(async move {
                    if let Err(err) =
                        crate::app::persist_ollama_num_ctx_for_model(&model_id, num_ctx)
                    {
                        tracing::warn!(error = %err, "failed to persist Ollama num_ctx for model");
                    }
                });
            },
            Cmd::PersistOllamaOffload(enabled) => {
                self.detached.spawn(async move {
                    if let Err(err) = crate::app::persist_ollama_allow_ram_offload(enabled) {
                        tracing::warn!(error = %err, "failed to persist Ollama RAM-offload setting");
                    }
                });
            },
            Cmd::PersistUiTheme(theme) => {
                self.detached.spawn(async move {
                    if let Err(err) = crate::app::persist_ui_theme(theme) {
                        tracing::warn!(error = %err, "failed to persist theme");
                    }
                });
            },
            Cmd::ComposeInEditor { .. } => {
                // Run-loop-intercepted in the interactive TUI (it owns the
                // terminal + event stream). Reaching the effect runner means a
                // headless driver emitted it — nothing to suspend there.
                tracing::warn!("compose_in_editor is unavailable outside the interactive TUI");
            },
            Cmd::ListMemory => {
                let tx = self.msg_tx.clone();
                let workdir = self.workdir.clone();
                self.detached.spawn(async move {
                    let cfg = crate::app::load_project_scoped_config(&workdir).memory;
                    let text = match crate::app::memory::load(&workdir, &cfg) {
                        Some(mem) => mem.index,
                        None => "No memories saved yet. Durable facts (yours or mine) show up here — use `/remember <fact>` or just ask me to remember something.".to_string(),
                    };
                    let _ = tx.send(Msg::RuntimeText(text)).await;
                });
            },
            Cmd::RememberMemory { text } => {
                let tx = self.msg_tx.clone();
                let workdir = self.workdir.clone();
                self.detached.spawn(async move {
                    let cfg = crate::app::load_project_scoped_config(&workdir).memory;
                    let name = memory_title_from_text(&text);
                    let status = match crate::app::memory::write_memory(
                        &workdir,
                        crate::app::memory::MemoryScope::ProjectPrivate,
                        &name,
                        &text,
                        &[],
                        &text,
                    ) {
                        Ok(_) => format!("Remembered: {name}"),
                        Err(e) => format!("Couldn't save memory: {e}"),
                    };
                    let (loaded, _) = crate::app::memory::refresh(None, &workdir, &cfg);
                    let _ = tx.send(Msg::MemoryChanged(loaded)).await;
                    let _ = tx.send(Msg::TransientStatus { text: status }).await;
                });
            },
            Cmd::ForgetMemory { id } => {
                let tx = self.msg_tx.clone();
                let workdir = self.workdir.clone();
                self.detached.spawn(async move {
                    let cfg = crate::app::load_project_scoped_config(&workdir).memory;
                    let status = match crate::app::memory::delete_memory(&workdir, &id) {
                        Ok(Some(_)) => format!("Forgot: {id}"),
                        Ok(None) => format!("No memory named '{id}'"),
                        Err(e) => format!("Couldn't forget memory: {e}"),
                    };
                    let (loaded, _) = crate::app::memory::refresh(None, &workdir, &cfg);
                    let _ = tx.send(Msg::MemoryChanged(loaded)).await;
                    let _ = tx.send(Msg::TransientStatus { text: status }).await;
                });
            },
            Cmd::ConsolidateMemory { model_id } => {
                let tx = self.msg_tx.clone();
                let workdir = self.workdir.clone();
                let providers = self.providers.clone();
                self.detached.spawn(async move {
                    consolidate_memory(tx, providers, workdir, model_id).await;
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
                            .list_conversation_metas()
                            .unwrap_or_default()
                            .into_iter()
                            .map(|m| crate::domain::ConversationSummary {
                                id: m.id,
                                title: m.title,
                                message_count: m.message_count,
                                updated_at: m.updated_at.to_rfc3339(),
                            })
                            .collect(),
                        Err(_) => Vec::new(),
                    };
                    let _ = tx.send(Msg::ConversationsListed(summaries)).await;
                });
            },
            Cmd::ListProjectFiles => {
                let tx = self.msg_tx.clone();
                let workdir = self.workdir.clone();
                // Filesystem walk — blocking pool, like the other sync I/O.
                self.detached.spawn_blocking(move || {
                    let files = walk_project_files(&workdir);
                    let _ = tx.blocking_send(Msg::ProjectFilesListed(files));
                });
            },
            Cmd::ListRuntimeTasks { limit } => {
                let tx = self.msg_tx.clone();
                // Synchronous rusqlite read — run on the blocking pool so it
                // never stalls an async worker thread (#40).
                self.detached.spawn_blocking(move || {
                    let tasks = crate::runtime::RuntimeClient::auto()
                        .list_tasks(limit)
                        .map(|read| read.value)
                        .unwrap_or_default();
                    let _ = tx.blocking_send(Msg::RuntimeTasksListed(tasks));
                });
            },
            Cmd::LoadRuntimeTask { id } => {
                let tx = self.msg_tx.clone();
                self.detached.spawn_blocking(move || {
                    let (task, events) = crate::runtime::RuntimeClient::auto()
                        .task_detail(&id)
                        .map(|read| (Some(read.value.task), read.value.events))
                        .unwrap_or((None, Vec::new()));
                    let _ = tx.blocking_send(Msg::RuntimeTaskLoaded { task, events });
                });
            },
            Cmd::ListRuntimeProcesses { limit } => {
                let tx = self.msg_tx.clone();
                self.detached.spawn_blocking(move || {
                    let processes = crate::runtime::RuntimeClient::auto()
                        .list_processes(limit)
                        .map(|read| read.value)
                        .unwrap_or_default();
                    let _ = tx.blocking_send(Msg::RuntimeProcessesListed(processes));
                });
            },
            Cmd::ShowRuntimeProcessLogs { id } => {
                let tx = self.msg_tx.clone();
                self.detached.spawn_blocking(move || {
                    let text = crate::runtime::RuntimeClient::auto()
                        .process_log(&id, None)
                        .map(|log| format!("Process log {}\n\n{}", id, log.content))
                        .unwrap_or_else(|err| format!("Process log error: {}", err));
                    let _ = tx.blocking_send(Msg::RuntimeText(text));
                });
            },
            Cmd::StopRuntimeProcess { id } => {
                let tx = self.msg_tx.clone();
                self.detached.spawn_blocking(move || {
                    let msg = match crate::runtime::RuntimeClient::auto().stop_process(&id) {
                        Ok(response) => Msg::TransientStatus {
                            text: format!("Stopped process {} (pid {})", id, response.item.pid),
                        },
                        Err(err) => Msg::TransientStatus {
                            text: format!("Process stop failed: {}", err),
                        },
                    };
                    let _ = tx.blocking_send(msg);
                });
            },
            Cmd::KillBackgroundAgent { agent_id } => {
                // Synchronous token fire — no task to spawn. Feedback flows
                // through the dying child's `Msg::BackgroundAgentFinished`
                // (the reducer already validated the id against its registry).
                let spawner = self.tools.as_ref().and_then(|t| t.subagent_spawner());
                if let Some(spawner) = spawner {
                    match agent_id {
                        Some(id) => {
                            spawner.kill_detached(&id);
                        },
                        None => {
                            spawner.kill_all_detached();
                        },
                    }
                }
            },
            Cmd::RestartRuntimeProcess { id } => {
                let tx = self.msg_tx.clone();
                self.detached.spawn_blocking(move || {
                    let msg = match crate::runtime::RuntimeClient::auto().restart_process(&id) {
                        Ok(response) => Msg::TransientStatus {
                            text: format!("Restarted process {} (pid {})", id, response.item.pid),
                        },
                        Err(err) => Msg::TransientStatus {
                            text: format!("Process restart failed: {}", err),
                        },
                    };
                    let _ = tx.blocking_send(msg);
                });
            },
            Cmd::OpenRuntimeTarget { target } => {
                self.detached.spawn_blocking(move || {
                    let resolved = crate::runtime::RuntimeService::open_default()
                        .and_then(|service| service.resolve_open_target(&target))
                        .unwrap_or(target);
                    // #63: the resolved value can be a `detected_url`/`log_path`
                    // from a `processes` row — validate before the OS opener,
                    // exactly like `open_process`.
                    if let Err(err) = crate::runtime::validate_open_target(&resolved) {
                        tracing::warn!(error = %err, "refusing to open runtime target");
                        return;
                    }
                    crate::utils::open_file(resolved);
                });
            },
            Cmd::ShowRuntimePorts => {
                let tx = self.msg_tx.clone();
                self.detached.spawn_blocking(move || {
                    let text = crate::runtime::RuntimeClient::auto()
                        .ports()
                        .map(|ports| format!("Listening TCP ports\n\n{}", ports.ports))
                        .unwrap_or_else(|err| format!("Port inspection failed: {}", err));
                    let _ = tx.blocking_send(Msg::RuntimeText(text));
                });
            },
            Cmd::ListRuntimeApprovals => {
                let tx = self.msg_tx.clone();
                self.detached.spawn_blocking(move || {
                    let approvals = crate::runtime::RuntimeClient::auto()
                        .list_approvals()
                        .map(|read| read.value)
                        .unwrap_or_default();
                    let _ = tx.blocking_send(Msg::RuntimeApprovalsListed(approvals));
                });
            },
            Cmd::DecideRuntimeApproval { id, decision } => {
                let tx = self.msg_tx.clone();
                self.detached.spawn_blocking(move || {
                    let result = if decision == "approved" {
                        crate::runtime::RuntimeClient::auto().approve(&id)
                    } else {
                        crate::runtime::RuntimeClient::auto().deny(&id)
                    };
                    let msg = match result {
                        Ok(result) => Msg::TransientStatus {
                            text: if result.replayed {
                                format!("Approval {} {}: {}", id, decision, result.summary)
                            } else {
                                format!("Approval {} {}", id, decision)
                            },
                        },
                        Err(err) => Msg::TransientStatus {
                            text: format!("Approval update failed: {}", err),
                        },
                    };
                    let _ = tx.blocking_send(msg);
                });
            },
            Cmd::ListRuntimeCheckpoints { limit } => {
                let tx = self.msg_tx.clone();
                self.detached.spawn_blocking(move || {
                    let checkpoints = crate::runtime::RuntimeClient::auto()
                        .list_checkpoints(limit)
                        .map(|read| read.value)
                        .unwrap_or_default();
                    let _ = tx.blocking_send(Msg::RuntimeCheckpointsListed(checkpoints));
                });
            },
            Cmd::ListForkCheckpoints {
                session_id,
                message_index,
            } => {
                let tx = self.msg_tx.clone();
                self.detached.spawn_blocking(move || {
                    let checkpoints = crate::runtime::RuntimeStore::open_default()
                        .and_then(|store| {
                            store
                                .checkpoints()
                                .list_for_session(&session_id, message_index as i64)
                        })
                        .unwrap_or_default();
                    let _ = tx.blocking_send(Msg::ForkCheckpointsFound(checkpoints));
                });
            },
            Cmd::ListRuntimePlugins => {
                let tx = self.msg_tx.clone();
                self.detached.spawn_blocking(move || {
                    let plugins = crate::runtime::RuntimeClient::auto()
                        .list_plugins()
                        .map(|read| read.value)
                        .unwrap_or_default();
                    let _ = tx.blocking_send(Msg::RuntimePluginsListed(plugins));
                });
            },
            Cmd::UpdateRuntimeTaskStatus {
                id,
                status,
                final_report,
            } => {
                let tx = self.msg_tx.clone();
                self.detached.spawn_blocking(move || {
                    let msg = match crate::runtime::RuntimeStore::open_default().and_then(|store| {
                        store
                            .tasks()
                            .update_status(&id, status, final_report.as_deref())
                    }) {
                        Ok(()) => Msg::TransientStatus {
                            text: format!("Task {} -> {}", id, status),
                        },
                        Err(err) => Msg::TransientStatus {
                            text: format!("Task update failed: {}", err),
                        },
                    };
                    let _ = tx.blocking_send(msg);
                });
            },
            Cmd::CreateRuntimeCheckpoint { paths } => {
                let tx = self.msg_tx.clone();
                let workdir = self.workdir.clone();
                self.detached.spawn_blocking(move || {
                    let pending_action = Some(serde_json::json!({
                        "source": "tui",
                        "command": "checkpoint",
                    }));
                    let msg =
                        match crate::runtime::create_checkpoint(&workdir, &paths, pending_action) {
                            Ok(manifest) => Msg::TransientStatus {
                                text: format!(
                                    "Checkpoint {} created for {} path(s)",
                                    manifest.id,
                                    manifest.files.len()
                                ),
                            },
                            Err(err) => Msg::TransientStatus {
                                text: format!("Checkpoint failed: {}", err),
                            },
                        };
                    let _ = tx.blocking_send(msg);
                });
            },
            Cmd::RestoreRuntimeCheckpoint { id } => {
                let tx = self.msg_tx.clone();
                self.detached.spawn_blocking(move || {
                    let msg = match crate::runtime::RuntimeClient::auto().restore_checkpoint(&id) {
                        Ok(result) => Msg::TransientStatus {
                            text: format!(
                                "Restored checkpoint {} ({} file(s)){}",
                                result.checkpoint.id,
                                result.checkpoint.files.len(),
                                if result.checkpoint.pending_action.is_some() {
                                    "; pending action available in checkpoint manifest"
                                } else {
                                    ""
                                }
                            ),
                        },
                        Err(err) => Msg::TransientStatus {
                            text: format!("Restore failed: {}", err),
                        },
                    };
                    let _ = tx.blocking_send(msg);
                });
            },
            Cmd::ShowRuntimeModelInfo { model } => {
                let tx = self.msg_tx.clone();
                self.detached.spawn_blocking(move || {
                    let text = runtime_model_info_text(&model);
                    let _ = tx.blocking_send(Msg::RuntimeText(text));
                });
            },
            Cmd::InitMcpServers(configs) => {
                let tx = self.msg_tx.clone();
                self.detached
                    .spawn(async move { dispatch_init_mcp_servers(configs, tx).await });
            },
            Cmd::StopMcpServer { name } => {
                let tx = self.msg_tx.clone();
                self.detached.spawn(async move {
                    // Actually kill the child before claiming it's stopped —
                    // otherwise the UI says "stopped" while the server runs on.
                    if let Some(mgr) = crate::mcp::manager_ref::get() {
                        mgr.stop_server(&name).await;
                    }
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
            Cmd::ProbeVision { model_id, warn } => {
                let tx = self.msg_tx.clone();
                let providers = self.providers.clone();
                self.detached.spawn(async move {
                    dispatch_probe_vision(model_id, warn, providers, tx).await;
                });
            },
            Cmd::CopyToClipboard(text) => {
                let tx = self.msg_tx.clone();
                self.detached.spawn(async move {
                    dispatch_copy_to_clipboard(text, tx).await;
                });
            },
            Cmd::Exit => {
                // The main loop observes `state.should_exit` after
                // the reducer returns; the runner doesn't need to
                // take any special action. Documented here for
                // exhaustiveness.
            },
            Cmd::SetTerminalTitle(title) => {
                if !self.terminal_title_enabled {
                    return;
                }
                // Offload the terminal write to the blocking pool: writing to
                // stdout can block when the terminal (or a downstream pipe) is
                // slow, and an async worker must not block on it (#44). The
                // OSC-2 title sequence is out-of-band relative to the renderer's
                // frame draws, so it doesn't corrupt them.
                self.detached.spawn_blocking(move || {
                    use std::io::Write;
                    let seq = format!("\x1b]2;{}\x07", title);
                    let mut stdout = std::io::stdout();
                    let _ = stdout.write_all(seq.as_bytes());
                    let _ = stdout.flush();
                });
            },
            Cmd::AlertUser => {
                if !self.terminal_title_enabled {
                    return;
                }
                // A single BEL nudges the terminal to alert (dock bounce / tab
                // highlight). Offloaded to the blocking pool like the title.
                self.detached.spawn_blocking(|| {
                    use std::io::Write;
                    let mut stdout = std::io::stdout();
                    let _ = stdout.write_all(b"\x07");
                    let _ = stdout.flush();
                });
            },
        }
    }

    fn queue_persistence(&mut self, job: PersistenceJob) {
        let previous = self.persistence_tail.take();
        let state = Arc::clone(&self.persistence_state);
        let tx = self.msg_tx.clone();
        self.persistence_tail = Some(tokio::spawn(async move {
            if let Some(previous) = previous
                && let Err(error) = previous.await
            {
                tracing::warn!(error = %error, "previous persistence job panicked");
            }

            let result = tokio::task::spawn_blocking(move || {
                state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .process(job)
            })
            .await;

            match result {
                Ok((events, outcome)) => {
                    // Events report durable writes even when the job as a
                    // whole failed — a partially drained barrier already
                    // persisted those archives, and they are never re-emitted.
                    if outcome.is_ok() || !events.is_empty() {
                        let _ = tx.send(Msg::SessionSaved).await;
                    }
                    for event in events {
                        fire_compaction_hook(&event).await;
                    }
                    if let Err(error) = outcome {
                        tracing::warn!(
                            error = %error,
                            "persistence job failed; compaction barriers remain queued"
                        );
                    }
                },
                Err(error) => tracing::warn!(error = %error, "persistence job panicked"),
            }
        }));
    }

    /// Async shutdown: cancel every scope, then wait for all spawned
    /// work to drain. Bounded by 5 seconds — a hung task past that
    /// gets aborted outright by `JoinSet::drop`.
    pub async fn shutdown(mut self) {
        for (id, scope) in self.scopes.iter() {
            tracing::debug!(turn = %id, "shutdown: cancelling scope");
            scope.cancel();
        }

        // The config watcher (#45) is a perpetual loop in `detached`; abort it
        // so the drain below doesn't block on it until the bounded timeout.
        if let Some(handle) = self.config_watch.take() {
            handle.abort();
        }

        // Drain with a bounded timeout.
        let shutdown_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);

        let owns_global_mcp = self.owns_global_mcp;
        let persistence_tail = self.persistence_tail.take();
        let persistence_state = Arc::clone(&self.persistence_state);
        let drain = async {
            if let Some(tail) = persistence_tail
                && let Err(error) = tail.await
            {
                tracing::warn!(error = %error, "shutdown: persistence chain panicked");
            }
            match tokio::task::spawn_blocking(move || {
                persistence_state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .retry_all_blocked()
            })
            .await
            {
                Ok((events, outcome)) => {
                    // A barrier drained at shutdown still owes its hooks —
                    // these events are never re-emitted.
                    for event in events {
                        fire_compaction_hook(&event).await;
                    }
                    if let Err(error) = outcome {
                        tracing::warn!(
                            error = %error,
                            "shutdown: compaction persistence barrier retry failed"
                        );
                    }
                },
                Err(error) => tracing::warn!(
                    error = %error,
                    "shutdown: compaction persistence barrier panicked"
                ),
            }
            // Only the top-level runner reaps the process-global MCP manager.
            // A subagent's child runner shares it; reaping here would kill the
            // parent's servers the moment the first subagent finished.
            if owns_global_mcp {
                // If an MCP init is still in flight, its child processes are
                // already spawned but `set_manager` hasn't run yet — `get()`
                // below would return `None` and we'd leak those children. Wait
                // (bounded) for init to settle so the manager is installed
                // before we reap it (#59).
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    crate::mcp::manager_ref::wait_ready(),
                )
                .await;
                // Gracefully shut down MCP server children (the stdin-EOF →
                // terminate → kill ladder in `McpServerManager::shutdown`). The
                // manager lives in a `'static OnceLock` that never drops, so
                // this explicit call on the exit path is the only thing that
                // reaps those child processes. No-op when no servers were
                // configured.
                if let Some(mgr) = crate::mcp::manager_ref::get() {
                    mgr.shutdown().await;
                }
                // Tear down the auto-managed SearXNG container (zero-config
                // web_search). Same ownership rule as MCP: only the top-level
                // runner reaps process-global services. No-op if none started.
                crate::searxng::shutdown().await;
            }
            // F42: bound each per-scope drain so one non-cooperative task can't
            // eat the whole shutdown budget and starve the remaining scopes'
            // drains (the scopes were all cancelled above, so a well-behaved task
            // unwinds well within this). On timeout, dropping `scope` aborts its
            // still-running `JoinSet` members via `TurnScope::drop`.
            for (id, mut scope) in self.scopes.drain() {
                if tokio::time::timeout(CANCEL_DRAIN_TIMEOUT, scope.drain())
                    .await
                    .is_err()
                {
                    tracing::warn!(
                        turn = %id,
                        timeout_ms = CANCEL_DRAIN_TIMEOUT.as_millis(),
                        "shutdown: scope drain timed out; aborting its remaining tasks"
                    );
                }
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
}

/// Dispatch a `CallModel` command. Resolves the provider (lazy,
/// cached) and streams its events onto the Msg channel. Without a
/// bound `ProviderFactory` (unit tests), emits a single
/// `UpstreamError` so the reducer ends the turn cleanly.
/// Report a completed request's completion tokens into the task broker's
/// cumulative counter, so task cost deltas (`tokens_spent`) can be computed
/// between in_progress and completed stamps.
fn note_stream_usage(
    tasks: &crate::providers::TaskBroker,
    usage: &Option<crate::models::TokenUsage>,
) {
    if let Some(usage) = usage {
        tasks.add_tokens(usage.completion_tokens as u64);
    }
}

async fn dispatch_call_model(
    msg_tx: MsgSender,
    providers: Option<Arc<ProviderFactory>>,
    turn: TurnId,
    mut request: crate::domain::ChatRequest,
    token: tokio_util::sync::CancellationToken,
    tasks: crate::providers::TaskBroker,
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
    {
        // Telemetry write — offload the synchronous DB upserts to the blocking
        // pool so they never stall this model-call dispatch path, which runs on
        // every turn (#39).
        let model_id = request.model_id.clone();
        let caps = provider.capabilities().clone();
        // Own this telemetry write inside the per-turn task (await it) instead of
        // a detached `spawn_blocking` whose handle was dropped — so a panic in the
        // upsert surfaces and shutdown isn't racing an untracked DB write (#F41).
        // It is a few-ms SQLite upsert before a multi-second model call, so
        // awaiting it here does not meaningfully stall the turn (the "never stall
        // dispatch" rule is about the synchronous reducer path, not this task).
        if let Err(e) =
            tokio::task::spawn_blocking(move || record_provider_capabilities(&model_id, &caps))
                .await
        {
            tracing::error!(error = %e, "effect: provider-capability telemetry write failed");
        }
    }
    if !request.tools.is_empty() && !provider.capabilities().supports_tools {
        let _ = msg_tx
            .send(Msg::TransientStatus {
                text: format!(
                    "{} does not advertise tool support; Mermaid will send the turn without tools",
                    request.model_id
                ),
            })
            .await;
        request.tools.clear();
    }

    // Resolve the *effective* context window. For Ollama this probes the model's
    // real window and auto-fits num_ctx to memory (cache-first, off the UI
    // thread); for other providers it's the static advertised window. Using the
    // effective value here is what un-skips auto-compaction for Ollama (which had
    // `NoKnownContextLimit`) and gives the status bar real numbers.
    let sizing = provider.resolve_context_window(&request).await;
    let max_context_tokens = sizing.effective.or_else(|| {
        crate::domain::runtime::infer_static_context_window_for_model_id(&request.model_id)
    });
    // Ride the discovered limits on the request itself so adapters size
    // `max_tokens` against the model's REAL window/ceiling (Anthropic
    // requires a concrete max_tokens; sizing it from a stale table either
    // wastes the ceiling or 400s). Set before the auto-compaction block so
    // `CompactionRequest::auto` inherits them for its summary calls.
    request.resolved_context_window = sizing.effective.or(sizing.model_max);
    request.resolved_max_output = sizing.max_output;
    // Report the resolved window to the reducer for the `/context` display +
    // truncation quick-fix. Harmless for non-Ollama (source is None → no extra
    // detail shown).
    let _ = msg_tx
        .send(Msg::ProviderContextResolved {
            model_id: request.model_id.clone(),
            model_max: sizing.model_max,
            effective: sizing.effective,
            source: sizing.source,
            max_output: sizing.max_output,
        })
        .await;
    // No-vision-model fallback: if this turn actually carries images, probe the
    // model's vision capability and let the reducer warn if it can't see them.
    // This backs up the proactive paste-time probe for the rare case where the
    // user pasted and sent before that probe resolved. Cheap — `supports_vision`
    // is cache-first, so a repeat probe in the same session is free.
    if request
        .messages
        .iter()
        .any(|m| m.images.as_ref().is_some_and(|v| !v.is_empty()))
    {
        let supports_vision = provider.supports_vision().await;
        let _ = msg_tx
            .send(Msg::ProviderVisionResolved {
                model_id: request.model_id.clone(),
                supports_vision,
                warn: true,
            })
            .await;
    }
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
        // Best-effort preflight: if there's nothing to compact, proceed
        // un-compacted (the provider's own context limit is the real gate).
        if let Ok(prepared) = crate::domain::prepare_compaction(&compaction, max_context_tokens) {
            match run_compaction(
                Arc::clone(&provider),
                turn,
                compaction,
                prepared,
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
                    // Auto-compaction is best-effort. If it can't reduce the
                    // context — the estimate is roughest exactly at the limit, so
                    // a large preserved tail can read `after >= before` — don't
                    // kill the turn. Log it, surface a soft warning, and proceed
                    // with the original request; the provider's own context limit
                    // is the real gate. (Manual `/compact` keeps its hard error
                    // via `run_compaction`'s reduction guard.)
                    if token.is_cancelled() {
                        return;
                    }
                    tracing::warn!(
                        turn = %turn,
                        error = %err,
                        "auto-compaction failed; proceeding with the un-compacted request",
                    );
                    let _ = msg_tx
                        .send(Msg::CompactionFailed {
                            turn,
                            trigger: CompactionTrigger::AutoThreshold,
                            message: err.to_string(),
                            kind: crate::domain::StatusKind::Warn,
                        })
                        .await;
                },
            }
        }
    }

    // Build a StreamContext — provider writes typed events into the
    // internal sink; we relay each to the reducer as a Msg.
    let (stream_tx, mut stream_rx) = mpsc::channel::<StreamEvent>(256);
    let ctx = StreamContext::new(token.clone(), stream_tx, turn);

    // Drain stream events into Msgs on a sibling task. Ends when the sink
    // closes (provider's final `Done` or completion) OR the turn token is
    // cancelled — `select!`ing on the token ties this relay to the turn's
    // structured cancellation so a cancel drops it within a tick instead of
    // waiting on the next event. (A separate task is required: the relay must
    // run concurrently with `provider.chat` for streaming backpressure.)
    let relay_tx = msg_tx.clone();
    let relay_token = token.clone();
    let relay_tasks = tasks.clone();
    let relay = spawn_guarded(async move {
        loop {
            let event = tokio::select! {
                biased;
                _ = relay_token.cancelled() => {
                    // #F40: a cancel landing right after the provider finished must
                    // not discard the terminal Done it already enqueued. Drain the
                    // buffered events and relay only a terminal Done — so the
                    // just-completed turn's usage is still recorded — while NOT
                    // painting buffered intermediate text (the turn is cancelled).
                    // `try_recv` drains the buffer without awaiting more.
                    while let Ok(buffered) = stream_rx.try_recv() {
                        if let StreamEvent::Done {
                            usage,
                            provider_continuation,
                            stop_reason,
                        } = buffered
                        {
                            note_stream_usage(&relay_tasks, &usage);
                            let _ = relay_tx
                                .send(Msg::StreamDone {
                                    turn,
                                    usage,
                                    provider_continuation,
                                    stop_reason,
                                })
                                .await;
                        }
                    }
                    break;
                },
                ev = stream_rx.recv() => match ev {
                    Some(ev) => ev,
                    None => break,
                },
            };
            let msg = match event {
                StreamEvent::Text(chunk) => Msg::StreamText { turn, chunk },
                StreamEvent::Reasoning(chunk) => Msg::StreamReasoning { turn, chunk },
                StreamEvent::ToolCall(call) => Msg::StreamToolCall { turn, call },
                // Plumbing notice ("Starting the local Ollama server…") —
                // a turn-independent system line, not response content.
                StreamEvent::Status(text) => Msg::TransientStatus { text },
                StreamEvent::Done {
                    usage,
                    provider_continuation,
                    stop_reason,
                } => {
                    note_stream_usage(&relay_tasks, &usage);
                    Msg::StreamDone {
                        turn,
                        usage,
                        provider_continuation,
                        stop_reason,
                    }
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
    let mut completed_ok = false;
    match provider.chat(request.clone(), ctx).await {
        Ok(_final_response) => {
            // Success — the final `Done` flowed through the sink.
            completed_ok = true;
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
                // Only retry if there's something to compact; otherwise fall
                // through to surface the original context-limit error.
                if let Ok(prepared) =
                    crate::domain::prepare_compaction(&compaction, max_context_tokens)
                {
                    match run_compaction(
                        Arc::clone(&provider),
                        turn,
                        compaction,
                        prepared,
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
                            join_logged(relay.take(), "stream_relay").await;
                            dispatch_provider_stream(
                                msg_tx,
                                provider,
                                turn,
                                retry_request,
                                token,
                                tasks,
                            )
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
            }
            let error = classify_error_for_ui(&e);
            run_provider_error_hook(&request.model_id, &error).await;
            let _ = msg_tx.send(Msg::UpstreamError { turn, error }).await;
        },
    }

    join_logged(relay.take(), "stream_relay").await;

    // Post-turn (success only): verify the model actually fit VRAM. Skipped when
    // the user allowed RAM offload (no warning possible) and a no-op for
    // non-Ollama providers (verify_placement returns None). Off the critical path
    // — StreamDone is already enqueued, so any warning renders after the answer.
    if completed_ok
        && request.ollama_allow_ram_offload != Some(true)
        && let Some(p) = provider.verify_placement(sizing.effective).await
    {
        tracing::debug!(
            size_vram_bytes = p.size_vram_bytes,
            total_bytes = p.total_bytes,
            offloaded = p.size_vram_bytes < p.total_bytes,
            suggested_num_ctx = ?p.suggested_num_ctx,
            "Ollama placement"
        );
        let _ = msg_tx
            .send(Msg::OllamaPlacementResolved {
                model_id: request.model_id.clone(),
                size_vram_bytes: p.size_vram_bytes,
                total_bytes: p.total_bytes,
                suggested_num_ctx: p.suggested_num_ctx,
            })
            .await;
    }
}

/// Drop-based per-turn model-call timer: emits a structured `tracing` event with
/// the elapsed wall time when the stream dispatch returns (success, error, or
/// cancel). Impure-shell only — lands in the log / TRACE bundle.
struct TurnTimer {
    turn: TurnId,
    model_id: String,
    started: std::time::Instant,
}

impl Drop for TurnTimer {
    fn drop(&mut self) {
        tracing::debug!(
            turn = %self.turn,
            model = %self.model_id,
            elapsed_ms = self.started.elapsed().as_millis() as u64,
            "model turn complete"
        );
    }
}

async fn dispatch_provider_stream(
    msg_tx: MsgSender,
    provider: Arc<dyn ModelProvider>,
    turn: TurnId,
    request: crate::domain::ChatRequest,
    token: tokio_util::sync::CancellationToken,
    tasks: crate::providers::TaskBroker,
) {
    let _turn_timer = TurnTimer {
        turn,
        model_id: request.model_id.clone(),
        started: std::time::Instant::now(),
    };
    let (stream_tx, mut stream_rx) = mpsc::channel::<StreamEvent>(256);
    let ctx = StreamContext::new(token.clone(), stream_tx, turn);
    let relay_tx = msg_tx.clone();
    let relay_token = token.clone();
    let relay_tasks = tasks.clone();
    let relay = spawn_guarded(async move {
        loop {
            let event = tokio::select! {
                biased;
                _ = relay_token.cancelled() => {
                    // #F40: a cancel landing right after the provider finished must
                    // not discard the terminal Done it already enqueued. Drain the
                    // buffered events and relay only a terminal Done — so the
                    // just-completed turn's usage is still recorded — while NOT
                    // painting buffered intermediate text (the turn is cancelled).
                    // `try_recv` drains the buffer without awaiting more.
                    while let Ok(buffered) = stream_rx.try_recv() {
                        if let StreamEvent::Done {
                            usage,
                            provider_continuation,
                            stop_reason,
                        } = buffered
                        {
                            note_stream_usage(&relay_tasks, &usage);
                            let _ = relay_tx
                                .send(Msg::StreamDone {
                                    turn,
                                    usage,
                                    provider_continuation,
                                    stop_reason,
                                })
                                .await;
                        }
                    }
                    break;
                },
                ev = stream_rx.recv() => match ev {
                    Some(ev) => ev,
                    None => break,
                },
            };
            let msg = match event {
                StreamEvent::Text(chunk) => Msg::StreamText { turn, chunk },
                StreamEvent::Reasoning(chunk) => Msg::StreamReasoning { turn, chunk },
                StreamEvent::ToolCall(call) => Msg::StreamToolCall { turn, call },
                // Plumbing notice — turn-independent system line.
                StreamEvent::Status(text) => Msg::TransientStatus { text },
                StreamEvent::Done {
                    usage,
                    provider_continuation,
                    stop_reason,
                } => {
                    note_stream_usage(&relay_tasks, &usage);
                    Msg::StreamDone {
                        turn,
                        usage,
                        provider_continuation,
                        stop_reason,
                    }
                },
            };
            if relay_tx.send(msg).await.is_err() {
                break;
            }
        }
    });

    let model_id = request.model_id.clone();
    match provider.chat(request, ctx).await {
        Ok(_) | Err(ModelError::Cancelled) => {},
        Err(e) => {
            let error = classify_error_for_ui(&e);
            run_provider_error_hook(&model_id, &error).await;
            let _ = msg_tx.send(Msg::UpstreamError { turn, error }).await;
        },
    }

    join_logged(relay.take(), "stream_relay").await;
}

/// Run plugin hooks OFF the async executor. `run_plugin_hooks` is synchronous —
/// it spawns hook children and bounded-waits on them — so calling it inline
/// would block a tokio worker, or (on the `dispatch` path) the whole event loop.
/// `spawn_blocking` moves it to the blocking pool. Hooks are fire-and-forget
/// observers, so the result is dropped.
async fn fire_plugin_hooks(event: &'static str, payload: serde_json::Value) {
    let _ = tokio::task::spawn_blocking(move || crate::runtime::run_plugin_hooks(event, &payload))
        .await;
}

/// Run hooks for an event whose responses GATE the action, returning the
/// aggregated verdict. Infrastructure failures (store/spawn errors, a panicked
/// blocking task) yield an empty gate — fail open; explicit hook denials
/// always deny.
async fn run_plugin_hooks_gated(
    event: &'static str,
    payload: serde_json::Value,
) -> crate::runtime::HookGate {
    tokio::task::spawn_blocking(move || {
        crate::runtime::run_plugin_hooks(event, &payload)
            .map(crate::runtime::aggregate_hook_responses)
            .unwrap_or_default()
    })
    .await
    .unwrap_or_default()
}

async fn run_provider_error_hook(model_id: &str, error: &crate::models::UserFacingError) {
    fire_plugin_hooks(
        "provider_error",
        serde_json::json!({
            "model_id": model_id,
            "summary": &error.summary,
            "message": &error.message,
            "category": format!("{:?}", error.category),
            "recoverable": error.recoverable,
        }),
    )
    .await;
}

/// Derive a short title for a `/remember` memory from free-text input: the
/// first non-empty line, capped to ~8 words / 60 chars. `write_memory`
/// slugifies it into the filename.
fn memory_title_from_text(text: &str) -> String {
    let first = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("memory")
        .trim();
    let title: String = first
        .split_whitespace()
        .take(8)
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(60)
        .collect();
    if title.trim().is_empty() {
        "memory".to_string()
    } else {
        title
    }
}

const CONSOLIDATE_SYSTEM_PROMPT: &str = "You maintain a coding agent's durable memory: a set of atomic facts. Your only job is to find facts that are EXACT DUPLICATES or CLEARLY OBSOLETE/SUPERSEDED by another fact, and list their ids for pruning. Never prune facts that are merely related or similar but carry distinct information. Never rewrite or merge facts. When in doubt, keep. Reply with ONLY a JSON object: {\"prune\": [\"id1\", \"id2\"], \"reason\": \"one short sentence\"}. If nothing should be pruned, return an empty prune list.";

#[derive(Debug)]
struct PrunePlan {
    prune: Vec<String>,
    reason: String,
}

/// Extract a `{prune:[...], reason:""}` plan from a model response, tolerating
/// prose or code fences around the JSON object.
fn parse_prune_plan(text: &str) -> Option<PrunePlan> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end < start {
        return None;
    }
    let json: serde_json::Value = serde_json::from_str(&text[start..=end]).ok()?;
    let prune = json
        .get("prune")?
        .as_array()?
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    let reason = json
        .get("reason")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    Some(PrunePlan { prune, reason })
}

/// `/consolidate-memory`: a one-shot model pass that names duplicate/obsolete
/// facts to prune (never rewrites — that's the anti-drift rule). The pruned
/// files are snapshotted into a checkpoint first, so the prune is reversible.
async fn consolidate_memory(
    tx: MsgSender,
    providers: Option<Arc<ProviderFactory>>,
    workdir: std::path::PathBuf,
    model_id: String,
) {
    let items = crate::app::memory::entries_with_bodies(&workdir);
    if items.len() < 2 {
        let _ = tx
            .send(Msg::RuntimeText(format!(
                "Nothing to consolidate — {} memor{} saved.",
                items.len(),
                if items.len() == 1 { "y" } else { "ies" }
            )))
            .await;
        return;
    }
    let Some(factory) = providers else {
        let _ = tx
            .send(Msg::RuntimeText(
                "Memory consolidation needs a model provider, which isn't bound in this session."
                    .to_string(),
            ))
            .await;
        return;
    };

    let mut listing = String::new();
    for (entry, body) in &items {
        let id = entry
            .path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(entry.name.as_str());
        listing.push_str(&format!(
            "- id: {id}\n  scope: {}\n  description: {}\n  body: {}\n",
            entry.scope.as_str(),
            entry.description,
            body.replace('\n', " ").trim(),
        ));
    }
    let user = format!(
        "Here are {} durable memory facts. Identify exact duplicates and clearly obsolete or superseded facts to prune.\n\n{}",
        items.len(),
        listing
    );
    let request = crate::domain::ChatRequest {
        model_id: model_id.clone(),
        messages: vec![crate::models::ChatMessage::user(user)],
        system_prompt: CONSOLIDATE_SYSTEM_PROMPT.to_string(),
        instructions: None,
        reasoning: crate::models::ReasoningLevel::None,
        temperature: 0.0,
        max_tokens: 1024,
        tools: Vec::new(),
        ollama_num_ctx: None,
        ollama_allow_ram_offload: None,
        resolved_context_window: None,
        resolved_max_output: None,
        output_schema: None,
        suppress_auto_compact: false,
    };

    let provider = match factory.resolve(&model_id).await {
        Ok(p) => p,
        Err(e) => {
            let _ = tx
                .send(Msg::RuntimeText(format!(
                    "Memory consolidation failed: {e}"
                )))
                .await;
            return;
        },
    };
    let token = tokio_util::sync::CancellationToken::new();
    let text =
        match crate::providers::model::collect_text(provider, TurnId(0), request, token).await {
            Ok((t, _)) => t,
            Err(e) => {
                let _ = tx
                    .send(Msg::RuntimeText(format!(
                        "Memory consolidation failed: {e}"
                    )))
                    .await;
                return;
            },
        };

    let Some(plan) = parse_prune_plan(&text) else {
        let _ = tx
            .send(Msg::RuntimeText(
                "Memory consolidation: couldn't parse the model's plan; nothing changed."
                    .to_string(),
            ))
            .await;
        return;
    };
    if plan.prune.is_empty() {
        let reason = if plan.reason.is_empty() {
            String::new()
        } else {
            format!(" {}", plan.reason)
        };
        let _ = tx
            .send(Msg::RuntimeText(format!(
                "Memory consolidation: nothing to prune.{reason}"
            )))
            .await;
        return;
    }

    // Snapshot the to-be-pruned files first so the prune is reversible. The
    // delete below is irreversible, so a failed checkpoint must NOT proceed —
    // otherwise the report would advertise "Recoverable from the latest
    // checkpoint" for a prune with no checkpoint behind it (#F69). Abort instead;
    // nothing has been deleted yet, so no memory is lost.
    let paths: Vec<std::path::PathBuf> = plan
        .prune
        .iter()
        .filter_map(|id| crate::app::memory::find(&workdir, id).map(|e| e.path))
        .collect();
    if !paths.is_empty()
        && let Err(e) = crate::runtime::create_checkpoint(
            &workdir,
            &paths,
            Some(serde_json::json!({ "tool": "consolidate_memory", "reason": plan.reason })),
        )
    {
        let _ = tx
            .send(Msg::RuntimeText(format!(
                "Memory consolidation aborted: couldn't checkpoint the {} file{} marked for pruning, so nothing was deleted (no memory lost). Error: {e}",
                paths.len(),
                if paths.len() == 1 { "" } else { "s" },
            )))
            .await;
        return;
    }

    let mut pruned = Vec::new();
    for id in &plan.prune {
        if let Ok(Some(_)) = crate::app::memory::delete_memory(&workdir, id) {
            pruned.push(id.clone());
        }
    }

    let cfg = crate::app::load_project_scoped_config(&workdir).memory;
    let (loaded, _) = crate::app::memory::refresh(None, &workdir, &cfg);
    let _ = tx.send(Msg::MemoryChanged(loaded)).await;

    let report = if pruned.is_empty() {
        "Memory consolidation: the model named facts to prune, but none matched existing memories."
            .to_string()
    } else {
        format!(
            "Consolidated memory — pruned {} fact{}: {}.{} Recoverable from the latest checkpoint (/checkpoints, /restore).",
            pruned.len(),
            if pruned.len() == 1 { "" } else { "s" },
            pruned.join(", "),
            if plan.reason.is_empty() {
                String::new()
            } else {
                format!(" Reason: {}.", plan.reason)
            },
        )
    };
    let _ = tx.send(Msg::RuntimeText(report)).await;
}

async fn dispatch_compact_conversation(
    msg_tx: MsgSender,
    providers: Option<Arc<ProviderFactory>>,
    turn: TurnId,
    mut request: CompactionRequest,
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

    // Resolve the window live (cache-first, so a manual /compact right after
    // a turn is a pure cache read). Static capabilities are `None` for
    // providers that discover limits at turn time (Anthropic/Gemini) — using
    // them here would regress manual /compact to "unknown window".
    let sizing = provider.resolve_context_window(&request.chat).await;
    request.chat.resolved_context_window = sizing.effective.or(sizing.model_max);
    request.chat.resolved_max_output = sizing.max_output;
    let max_context_tokens = request.chat.resolved_context_window.or_else(|| {
        crate::domain::runtime::infer_static_context_window_for_model_id(&request.chat.model_id)
    });
    let before_snapshot =
        crate::domain::estimate_context_usage_for_request(&request.chat, max_context_tokens);

    let trigger = request.trigger;
    // A benign precondition (e.g. too little history to summarize) is a no-op, not
    // a failure — surface it as `Info` so the reducer shows a calm note instead of
    // a "Compaction failed: Invalid request" error. Real failures (model errors,
    // an empty/non-reducing summary) still flow through `run_compaction` as errors.
    let prepared = match crate::domain::prepare_compaction(&request, max_context_tokens) {
        Ok(prepared) => prepared,
        Err(skip) => {
            let _ = msg_tx
                .send(Msg::CompactionFailed {
                    turn,
                    trigger,
                    message: skip.to_string(),
                    kind: crate::domain::StatusKind::Info,
                })
                .await;
            return;
        },
    };
    match run_compaction(
        provider,
        turn,
        request,
        prepared,
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
    prepared: crate::domain::PreparedCompaction,
    before_snapshot: crate::domain::ContextUsageSnapshot,
    max_context_tokens: Option<usize>,
    token: tokio_util::sync::CancellationToken,
) -> Result<CompactionResult, ModelError> {
    let started = Instant::now();

    let summary_request = crate::domain::build_summary_request(
        &request.chat,
        &prepared,
        request.instructions.as_deref(),
        request.policy,
    );
    ensure_compaction_request_fits(&summary_request, max_context_tokens)?;
    let (draft, draft_usage) =
        collect_compaction_text(Arc::clone(&provider), turn, summary_request, token.clone())
            .await?;
    let draft_summary = crate::domain::normalize_summary(&draft);
    let draft_validation = crate::domain::validate_summary_structure(&draft_summary);

    let verify_request = crate::domain::build_verification_request(
        &request.chat,
        &prepared,
        &draft_summary,
        request.instructions.as_deref(),
        request.policy,
    );
    let review_fits = compaction_request_fits(&verify_request, max_context_tokens);
    let (final_summary, verify_usage, review_status, review_error) = if review_fits {
        match collect_compaction_text(Arc::clone(&provider), turn, verify_request, token).await {
            Ok((verified_text, verify_usage)) => {
                let verified_summary = crate::domain::normalize_summary(&verified_text);
                match crate::domain::validate_summary_structure(&verified_summary) {
                    Ok(()) => (
                        verified_summary,
                        verify_usage,
                        crate::domain::CompactionReviewStatus::Reviewed,
                        None,
                    ),
                    Err(error) => match draft_validation {
                        Ok(()) => (
                            draft_summary,
                            verify_usage,
                            crate::domain::CompactionReviewStatus::DraftValidated,
                            Some(format!("review returned an invalid checkpoint: {error}")),
                        ),
                        Err(draft_error) => {
                            return Err(ModelError::InvalidRequest(format!(
                                "compaction produced no structurally valid checkpoint (draft: {draft_error}; review: {error})"
                            )));
                        },
                    },
                }
            },
            Err(ModelError::Cancelled) => return Err(ModelError::Cancelled),
            Err(err) => match draft_validation {
                Ok(()) => (
                    draft_summary,
                    None,
                    crate::domain::CompactionReviewStatus::DraftValidated,
                    Some(format!("review failed: {err}")),
                ),
                Err(draft_error) => {
                    return Err(ModelError::InvalidRequest(format!(
                        "compaction draft was invalid and review failed (draft: {draft_error}; review: {err})"
                    )));
                },
            },
        }
    } else {
        match draft_validation {
            Ok(()) => (
                draft_summary,
                None,
                crate::domain::CompactionReviewStatus::DraftValidated,
                Some(
                    "review skipped because the complete request would exceed the context window"
                        .to_string(),
                ),
            ),
            Err(error) => {
                return Err(ModelError::InvalidRequest(format!(
                    "compaction draft was invalid and the review request did not fit: {error}"
                )));
            },
        }
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
        preserved_turn_count: prepared
            .preserved_messages
            .iter()
            .filter(|message| message.role == crate::models::MessageRole::User)
            .count(),
        summary_tokens: final_summary.len().div_ceil(4),
        duration_secs: started.elapsed().as_secs_f64(),
        review_status,
        review_error,
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
        source_boundaries: request
            .chat
            .messages
            .iter()
            .map(crate::domain::CompactionBoundary::from_message)
            .collect(),
    })
}

fn compaction_request_fits(
    request: &crate::domain::ChatRequest,
    max_context_tokens: Option<usize>,
) -> bool {
    let Some(max_tokens) = max_context_tokens else {
        return true;
    };
    let used = crate::domain::estimate_context_usage_for_request(request, Some(max_tokens));
    used.used_tokens.saturating_add(request.max_tokens) <= max_tokens
}

fn ensure_compaction_request_fits(
    request: &crate::domain::ChatRequest,
    max_context_tokens: Option<usize>,
) -> Result<(), ModelError> {
    if compaction_request_fits(request, max_context_tokens) {
        Ok(())
    } else {
        Err(ModelError::InvalidRequest(
            "complete compaction request exceeds the model context window".to_string(),
        ))
    }
}

async fn collect_compaction_text(
    provider: Arc<dyn ModelProvider>,
    turn: TurnId,
    request: crate::domain::ChatRequest,
    token: tokio_util::sync::CancellationToken,
) -> Result<(String, Option<TokenUsage>), ModelError> {
    // Shared with the Auto-mode safety classifier — see
    // `crate::providers::model::collect_text`.
    crate::providers::model::collect_text(provider, turn, request, token).await
}

fn record_provider_capabilities(
    model_id: &str,
    caps: &crate::providers::capabilities::Capabilities,
) {
    let (provider, model) = split_model_id(model_id);
    if let Ok(store) = crate::runtime::RuntimeStore::open_default() {
        for (key, value) in [
            ("tools_support", caps.supports_tools.to_string()),
            ("vision_support", caps.supports_vision.to_string()),
            (
                "context_limit",
                caps.max_context_tokens
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "unknown".to_string()),
            ),
            (
                "reasoning_parameter_shape",
                format!("{:?}", caps.supports_reasoning),
            ),
            (
                "streaming_usage_available",
                "provider_dependent".to_string(),
            ),
            ("token_usage_field_shape", "normalized".to_string()),
        ] {
            let _ = store
                .provider_probes()
                .upsert(crate::runtime::NewProviderProbe {
                    provider: provider.clone(),
                    model_id: model.clone(),
                    capability_key: key.to_string(),
                    capability_value: value,
                    confidence: "verified".to_string(),
                    error: None,
                });
        }
    }
}

fn split_model_id(model_id: &str) -> (String, String) {
    match model_id.split_once('/') {
        Some((provider, model)) if !provider.is_empty() && !model.is_empty() => {
            (provider.to_ascii_lowercase(), model.to_string())
        },
        _ => ("ollama".to_string(), model_id.to_string()),
    }
}

/// Hard cap on paths returned by [`walk_project_files`]. Well past any
/// project the picker is useful on; keeps a runaway monorepo walk bounded.
const MAX_PROJECT_FILES: usize = 20_000;

/// Enumerate the project for the @-mention picker: gitignore-aware
/// (ripgrep's walker — .gitignore/.ignore/global excludes), hidden entries
/// and `.git` skipped, symlinks not followed. Returns RELATIVE UTF-8 paths
/// sorted lexicographically, directories with a trailing `/`, capped at
/// [`MAX_PROJECT_FILES`]. Non-UTF-8 paths are skipped — the mention is
/// spliced into the text prompt, so it must be valid text.
fn walk_project_files(root: &std::path::Path) -> Vec<String> {
    let mut files = Vec::new();
    for entry in ignore::WalkBuilder::new(root)
        .hidden(true)
        .follow_links(false)
        .build()
        .flatten()
    {
        if files.len() >= MAX_PROJECT_FILES {
            break;
        }
        let path = entry.path();
        if path == root {
            continue;
        }
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        let Some(mut rel) = rel.to_str().map(str::to_string) else {
            continue;
        };
        // Normalize Windows separators so a mention is stable text.
        if std::path::MAIN_SEPARATOR != '/' {
            rel = rel.replace(std::path::MAIN_SEPARATOR, "/");
        }
        if entry.file_type().is_some_and(|t| t.is_dir()) {
            rel.push('/');
        }
        files.push(rel);
    }
    files.sort();
    files
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
    background: tokio_util::sync::CancellationToken,
    config: Arc<crate::app::Config>,
    model_id: String,
    task_id: Option<String>,
    session_id: String,
    message_index: usize,
    scratchpad: Option<PathBuf>,
    safety_mode: crate::runtime::SafetyMode,
    plan_file: Option<PathBuf>,
    plan_permissions: crate::app::PlanPermissions,
    context_percent: Option<u8>,
    intent: Option<String>,
    classifier: Option<Arc<dyn crate::providers::AutoClassifier>>,
    approval: Option<crate::providers::ApprovalBroker>,
    questions: Option<crate::providers::QuestionBroker>,
    tasks: crate::providers::TaskBroker,
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
    let tool_run_id =
        start_runtime_tool_run(task_id.as_deref(), turn, call_id, tool_key, &args).await;

    let Some(tool) = registry.get(tool_key) else {
        let outcome = crate::domain::ToolOutcome::error(format!("unknown tool: {}", tool_key), 0.0);
        finish_runtime_tool_run(tool_run_id.as_deref(), &outcome);
        let _ = msg_tx
            .send(Msg::ToolFinished {
                turn,
                call_id,
                outcome,
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
    let relay_token = token.clone();
    let progress_relay = spawn_guarded(async move {
        loop {
            let event = tokio::select! {
                biased;
                _ = relay_token.cancelled() => break,
                ev = progress_rx.recv() => match ev {
                    Some(ev) => ev,
                    None => break,
                },
            };
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

    let mut ctx = ExecContext::new(
        token,
        progress_tx,
        call_id,
        turn,
        workdir,
        config,
        model_id,
        task_id,
        Some(session_id),
        Some(message_index as i64),
        safety_mode,
        intent,
        classifier,
        approval,
        questions,
        Some(tasks.clone()),
    );
    ctx.background = background;
    ctx.plan_file = plan_file;
    ctx.plan_permissions = plan_permissions;
    ctx.context_percent = context_percent;
    // Detached work (backgrounded subagents) reports back through the main
    // msg channel after this turn's progress relay is gone.
    ctx.notify = Some(msg_tx.clone());
    // Per-session scratch dir, when the session has one materialized.
    ctx.scratchpad = scratchpad;
    // `before_tool_use` is the one DECISION event: an enabled plugin hook may
    // deny the call, rewrite its arguments, or inject context for the next
    // model request. Every other event stays fire-and-forget.
    let before_payload = serde_json::json!({
        "turn_id": turn.0,
        "call_id": call_id.0,
        "tool": tool_key,
        "arguments": args,
    });
    let gate = run_plugin_hooks_gated("before_tool_use", before_payload).await;
    if !gate.context.is_empty() {
        // Injected context flows into transcripts/model input — scrub
        // credential-shaped content on the way in.
        let texts = gate
            .context
            .iter()
            .map(|t| crate::utils::redact_secrets(t))
            .collect();
        let _ = msg_tx.send(Msg::HookContext { turn, texts }).await;
    }
    if let Some((plugin, reason)) = gate.deny {
        // Mirror the unknown-tool arm: synthesize an error outcome and unwind.
        // Dropping `ctx` closes the progress channel so the relay terminates
        // before the join below.
        drop(ctx);
        let reason = crate::utils::redact_secrets(&reason);
        let outcome = crate::domain::ToolOutcome::error(
            format!("Denied by plugin hook ({plugin}): {reason}"),
            0.0,
        );
        finish_runtime_tool_run(tool_run_id.as_deref(), &outcome);
        join_logged(progress_relay.take(), "tool_progress_relay").await;
        let _ = msg_tx
            .send(Msg::ToolFinished {
                turn,
                call_id,
                outcome,
            })
            .await;
        return;
    }
    // A rewritten input is deliberately NOT redacted (it becomes executable
    // args — corrupting them would be worse), and it cannot launder a blocked
    // action: the policy gate runs inside `tool.execute` and vets the
    // rewritten call exactly like an original one.
    let args = gate.updated_input.unwrap_or(args);
    let outcome = tool.execute(args, ctx).await;
    // Evidence trail: attribute this call to the in-progress checklist task
    // (no-op when none). The task tools themselves are skipped — a checklist
    // edit is not evidence of work on the task. `display_info_for` gives the
    // same human target the transcript row shows (path, command head, query).
    if !source.function.name.starts_with("task_") {
        let (action, target) = crate::domain::display_info_for(&crate::domain::PendingToolCall {
            call_id,
            source: source.clone(),
        });
        tasks
            .record_evidence(crate::domain::EvidenceEntry {
                tool: action,
                target,
                status: tool_status_label(outcome.status).to_string(),
            })
            .await;
    }
    let after_payload = serde_json::json!({
        "turn_id": turn.0,
        "call_id": call_id.0,
        "tool": tool_key,
        "status": tool_status_label(outcome.status),
        "summary": &outcome.summary,
    });
    fire_plugin_hooks("after_tool_use", after_payload).await;
    finish_runtime_tool_run(tool_run_id.as_deref(), &outcome);
    join_logged(progress_relay.take(), "tool_progress_relay").await;
    let _ = msg_tx
        .send(Msg::ToolFinished {
            turn,
            call_id,
            outcome,
        })
        .await;
}

async fn start_runtime_tool_run(
    task_id: Option<&str>,
    turn: TurnId,
    call_id: crate::domain::ToolCallId,
    tool_name: &str,
    args: &serde_json::Value,
) -> Option<String> {
    // Synchronous rusqlite write on the hot tool-execution path — offload it to
    // the blocking pool. The id is needed by `finish`, so we await the result
    // (unlike `finish`, which is fire-and-forget) (#39).
    let task_id = task_id.map(str::to_string);
    let tool_name = tool_name.to_string();
    let args_json = serde_json::to_string(args).ok();
    tokio::task::spawn_blocking(move || {
        crate::runtime::RuntimeStore::open_default()
            .and_then(|store| {
                store.tool_runs().start(crate::runtime::NewToolRun {
                    id: None,
                    task_id,
                    turn_id: Some(turn.0.to_string()),
                    call_id: Some(call_id.0.to_string()),
                    tool_name,
                    args_json,
                })
            })
            .map(|record| record.id)
            .ok()
    })
    .await
    .ok()
    .flatten()
}

fn finish_runtime_tool_run(tool_run_id: Option<&str>, outcome: &crate::domain::ToolOutcome) {
    let Some(tool_run_id) = tool_run_id else {
        return;
    };
    let tool_run_id = tool_run_id.to_string();
    let status = tool_status_label(outcome.status).to_string();
    let output_json = serde_json::to_string(&serde_json::json!({
        "status": tool_status_label(outcome.status),
        "summary": &outcome.summary,
        "model_content": &outcome.model_content,
        "error": &outcome.error,
        "metadata": &outcome.metadata,
        "artifacts": &outcome.artifacts,
        "duration_secs": outcome.duration_secs,
    }))
    .ok();
    // Fire-and-forget telemetry write on the blocking pool — don't stall the
    // tool-finish path waiting on rusqlite (#39).
    tokio::task::spawn_blocking(move || {
        if let Ok(store) = crate::runtime::RuntimeStore::open_default() {
            let _ = store
                .tool_runs()
                .finish(&tool_run_id, &status, output_json.as_deref());
        }
    });
}

fn tool_status_label(status: crate::domain::ToolStatus) -> &'static str {
    match status {
        crate::domain::ToolStatus::Success => "success",
        crate::domain::ToolStatus::Error => "error",
        crate::domain::ToolStatus::Cancelled => "cancelled",
    }
}

fn runtime_model_info_text(model: &str) -> String {
    let snapshot = crate::domain::runtime::ProviderCapabilitySnapshot::from_model_id(model);
    let mut lines = vec![
        format!("Model info: {}", model),
        format!("- provider: {}", snapshot.provider),
        format!("- model: {}", snapshot.model),
        format!("- supports tools: {}", snapshot.supports_tools),
        format!("- supports vision: {}", snapshot.supports_vision),
        format!("- reasoning: {}", snapshot.reasoning),
        format!(
            "- context limit: {}",
            snapshot
                .max_context_tokens
                .map(|value: usize| value.to_string())
                .unwrap_or_else(|| "unknown".to_string())
        ),
    ];
    if let Ok(store) = crate::runtime::RuntimeStore::open_default()
        && let Ok(probes) = store
            .provider_probes()
            .list(Some(&snapshot.provider), Some(&snapshot.model))
        && !probes.is_empty()
    {
        lines.push(String::new());
        lines.push("Cached provider reality records:".to_string());
        for probe in probes {
            lines.push(format!(
                "- {} = {} ({})",
                probe.capability_key, probe.capability_value, probe.confidence
            ));
        }
    }
    lines.join("\n")
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

    // Capture the reader's handle instead of orphaning it: the child's stdout
    // closes when it exits, so this task finishes right after `child.wait`
    // below — we join it there so a panic is logged, not silently lost (#60).
    let reader_handle = child.stdout.take().map(|stdout| {
        let tx_inner = tx.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                let _ = tx_inner.send(Msg::ModelPullProgress(line)).await;
            }
        })
    });

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

    // The child has exited; its stdout is closed, so the reader is finishing.
    // Join it (logging a panic) so it isn't left orphaned (#60).
    if let Some(handle) = reader_handle {
        join_logged(handle, "ollama_pull_reader").await;
    }
}

/// Start every configured MCP server CONCURRENTLY, each bounded by
/// `MCP_STARTUP_TIMEOUT`, emitting one `Msg::McpServerReady`/`McpServerErrored`
/// per server AS IT RESOLVES — a slow server never delays the rest. The
/// (initially empty) manager is installed BEFORE the tasks spawn so shutdown
/// always finds it; init is "complete" once every server has resolved
/// (`McpToolProxy::wait_ready` semantics unchanged — a first-message
/// `mcp__` call waits, bounded, for the full fleet). A zero-tool server that
/// started successfully is still Ready with an empty tool list.
async fn dispatch_init_mcp_servers(
    configs: std::collections::HashMap<String, crate::app::McpServerConfig>,
    tx: tokio::sync::mpsc::Sender<Msg>,
) {
    if configs.is_empty() {
        return;
    }
    crate::mcp::manager_ref::mark_init_started();
    let manager = std::sync::Arc::new(crate::mcp::McpServerManager::new(&configs));
    crate::mcp::manager_ref::set_manager(manager.clone());
    let mut join = tokio::task::JoinSet::new();
    for (name, config) in configs {
        let manager = manager.clone();
        let tx = tx.clone();
        join.spawn(async move {
            let msg = match manager.start_server(&name, &config).await {
                Ok(tools) => Msg::McpServerReady { name, tools },
                Err(e) => Msg::McpServerErrored {
                    name,
                    reason: e.to_string(),
                },
            };
            let _ = tx.send(msg).await;
        });
    }
    while join.join_next().await.is_some() {}
    crate::mcp::manager_ref::mark_init_complete();
}

/// Read the system clipboard on a blocking thread and emit a `Msg`
/// back into the main loop. Image content wins when present; falls
/// back to text; empty or error surface as `Msg::TransientStatus` so
/// the user gets visible feedback (a silent no-op on Ctrl+V would be
/// confusing, especially on macOS where `osascript` can take ~300ms).
///
/// `tokio::task::spawn_blocking` is the right primitive: `clipboard::
/// has_image` / `read_image_bytes` / `read_text` shell out to xclip /
/// wl-paste / pngpaste / PowerShell, all of which block synchronously —
/// bounded, since every clipboard subprocess runs under a kill-on-timeout
/// deadline, so a hung helper returns an error here instead of pinning
/// this blocking thread forever.
async fn dispatch_read_clipboard(tx: MsgSender) {
    use crate::domain::ClipboardRead;

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

    // Route ALL four outcomes through `Msg::ClipboardRead` (not `Msg::Paste` /
    // `Msg::TransientStatus`): the reducer decrements `clipboard_reads_pending`
    // on exactly these messages, so an empty/failed read must still land here to
    // release a submit that was held waiting on it.
    let msg = match outcome {
        Outcome::Image { bytes, format } => {
            Msg::ClipboardRead(ClipboardRead::Image { bytes, format })
        },
        Outcome::Text(text) => Msg::ClipboardRead(ClipboardRead::Text(text)),
        Outcome::Empty => Msg::ClipboardRead(ClipboardRead::Empty),
        Outcome::Error(text) => Msg::ClipboardRead(ClipboardRead::Error(text)),
    };
    let _ = tx.send(msg).await;
}

/// Probe whether `model_id` can see images and report it via
/// `Msg::ProviderVisionResolved`. Best-effort: an unresolvable provider or a
/// provider that doesn't probe (non-Ollama) reports `None` ("unknown"), which
/// the reducer treats as "don't warn". `warn` rides through unchanged so the
/// reducer knows whether an image is actually in play.
async fn dispatch_probe_vision(
    model_id: String,
    warn: bool,
    providers: Option<Arc<ProviderFactory>>,
    tx: MsgSender,
) {
    let supports_vision = match providers {
        Some(factory) => match factory.resolve(&model_id).await {
            Ok(provider) => provider.supports_vision().await,
            Err(_) => None,
        },
        None => None,
    };
    let _ = tx
        .send(Msg::ProviderVisionResolved {
            model_id,
            supports_vision,
            warn,
        })
        .await;
}

/// Write text to the system clipboard on a blocking thread (the platform
/// tools shell out and block), then report the result via a transient status.
async fn dispatch_copy_to_clipboard(text: String, tx: MsgSender) {
    let char_count = text.chars().count();
    let result = tokio::task::spawn_blocking(move || crate::clipboard::write_text(&text))
        .await
        .unwrap_or_else(|e| Err(anyhow::anyhow!("clipboard spawn_blocking: {e}")));

    let msg = match result {
        Ok(()) => Msg::TransientStatus {
            text: format!("Copied {char_count} chars to clipboard"),
        },
        Err(e) => Msg::TransientStatus {
            text: format!("Copy failed: {e}"),
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
        ModelError::RateLimit {
            retry_after,
            message,
        } => UserFacingError {
            summary: "Rate limited".to_string(),
            // The provider's own reason distinguishes "slow down" from
            // "daily quota exhausted" — show it when the 429 body had one.
            message: message.clone().unwrap_or_else(|| {
                "The provider rejected the request with 429 (too many requests).".to_string()
            }),
            suggestion: match retry_after {
                Some(secs) => format!("The provider asked to retry after {secs}s."),
                None => "Retry shortly; if it persists, check your plan's quota.".to_string(),
            },
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

    #[test]
    fn project_walk_respects_gitignore_sorts_and_marks_dirs() {
        let root = std::env::temp_dir().join(format!(
            "mermaid-walk-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(root.join("target/out.bin"), "ignored").unwrap();
        std::fs::write(root.join("README.md"), "readme").unwrap();
        std::fs::write(root.join(".hidden"), "hidden").unwrap();

        let files = walk_project_files(&root);
        assert_eq!(
            files,
            vec![
                "README.md".to_string(),
                "src/".to_string(),
                "src/main.rs".to_string(),
            ],
            "sorted, dirs slash-marked, target/ ignored, dotfiles hidden"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn new_child_suppresses_terminal_title() {
        // A subagent's child runner must not emit OSC 2 terminal titles —
        // otherwise they leak into a headless parent's stdout and corrupt
        // `--format json`/`text` output (caught during live headless testing).
        let (tx, _rx) = mpsc::channel::<Msg>(MSG_CHANNEL_CAPACITY);
        let providers = Arc::new(ProviderFactory::new(crate::app::Config::default()));
        let tools = Arc::new(ToolRegistry::new());
        let child = EffectRunner::new_child(tx, PathBuf::from("/tmp"), providers, tools);
        assert!(
            !child.terminal_title_enabled,
            "subagent child runner must suppress terminal-title escapes"
        );
    }

    #[test]
    fn new_child_does_not_own_global_mcp_shutdown() {
        // The MCP manager is process-global and shared with the parent. A
        // child runner's shutdown (which runs after EVERY subagent) must not
        // reap it — that would kill the parent's MCP servers for the rest of
        // the session. Only the top-level runner owns the reap.
        let (tx, _rx) = mpsc::channel::<Msg>(MSG_CHANNEL_CAPACITY);
        let providers = Arc::new(ProviderFactory::new(crate::app::Config::default()));
        let tools = Arc::new(ToolRegistry::new());
        let child = EffectRunner::new_child(tx, PathBuf::from("/tmp"), providers, tools);
        assert!(
            !child.owns_global_mcp,
            "child runner must not reap the shared global MCP manager"
        );
        let (top, _rx2) = EffectRunner::pair(PathBuf::from("/tmp"));
        assert!(
            top.owns_global_mcp,
            "top-level runner still owns the global MCP reap"
        );
    }

    #[test]
    fn parse_prune_plan_extracts_json_amid_prose() {
        let plan = parse_prune_plan(
            "Sure, here's the plan:\n```json\n{\"prune\": [\"a\", \"b\"], \"reason\": \"dupes\"}\n```\nDone.",
        )
        .expect("should parse");
        assert_eq!(plan.prune, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(plan.reason, "dupes");
    }

    #[test]
    fn parse_prune_plan_handles_empty_and_garbage() {
        let empty = parse_prune_plan("{\"prune\": [], \"reason\": \"all distinct\"}")
            .expect("empty plan parses");
        assert!(empty.prune.is_empty());
        assert!(parse_prune_plan("no json here").is_none());
    }

    #[test]
    fn memory_title_from_text_is_short_and_nonempty() {
        assert_eq!(
            memory_title_from_text("prefer ripgrep over grep"),
            "prefer ripgrep over grep"
        );
        assert_eq!(memory_title_from_text("   "), "memory");
        let long = memory_title_from_text("one two three four five six seven eight nine ten");
        assert!(long.split_whitespace().count() <= 8);
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
            crate::session::ConversationHistory::new(
                "/p".to_string(),
                "m".to_string(),
                chrono::Local::now(),
            ),
        ));
        let msg = tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .expect("sender emits")
            .expect("channel alive");
        assert!(matches!(msg, Msg::SessionSaved));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn init_mcp_servers_emits_incremental_errored_msgs() {
        // Two servers that both fail fast (nonexistent binaries): each
        // resolves independently and emits its own Errored msg; init
        // completes after both. Also exercises the empty-manager install.
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let mut configs = std::collections::HashMap::new();
        for name in ["one", "two"] {
            configs.insert(
                name.to_string(),
                crate::app::McpServerConfig {
                    command: "/nonexistent/mermaid-test-mcp-binary".to_string(),
                    ..Default::default()
                },
            );
        }
        dispatch_init_mcp_servers(configs, tx).await;
        let mut errored = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            match msg {
                Msg::McpServerErrored { name, .. } => errored.push(name),
                other => panic!("unexpected msg: {other:?}"),
            }
        }
        errored.sort();
        assert_eq!(errored, vec!["one".to_string(), "two".to_string()]);
        assert!(crate::mcp::manager_ref::is_ready());
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
    async fn cancel_scope_emits_turn_cancelled_even_after_reaping() {
        // Regression (Axis 1 #9): if a turn's tasks complete and
        // `reap_empty_scopes` removes the now-empty scope before the user's
        // cancel lands, `drop_scope` used to be a silent no-op and the reducer
        // stuck forever in `Cancelling`. The terminal `TurnCancelled` must fire
        // even when the scope is already gone.
        let (mut r, mut rx) = runner();
        let turn = TurnId(88);
        {
            let scope = r.scope_mut(turn);
            scope.spawn(async {}); // completes immediately
        }
        assert_eq!(r.scope_count(), 1);

        // Let the task finish, then any dispatch reaps the now-empty scope.
        tokio::time::sleep(Duration::from_millis(20)).await;
        r.dispatch(Cmd::Exit);
        assert_eq!(r.scope_count(), 0, "completed scope should be reaped");

        // The scope is gone, but the reducer is still `Cancelling`: cancel must
        // still produce a terminal message.
        r.dispatch(Cmd::CancelScope(turn));
        let msg = tokio::time::timeout(Duration::from_millis(500), rx.recv())
            .await
            .expect("cancel on a reaped scope must still emit a terminal message")
            .expect("channel alive");
        assert!(matches!(msg, Msg::TurnCancelled(t) if t == turn));
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

            ollama_num_ctx: None,
            ollama_allow_ram_offload: None,
            resolved_context_window: None,
            resolved_max_output: None,
            output_schema: None,
            suppress_auto_compact: false,
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

            ollama_num_ctx: None,
            ollama_allow_ram_offload: None,
            resolved_context_window: None,
            resolved_max_output: None,
            output_schema: None,
            suppress_auto_compact: false,
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
        r.dispatch(Cmd::SetTerminalTitle("x".to_string()));
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
            safety_mode: crate::runtime::SafetyMode::Ask,
            plan_file: None,
            plan_permissions: crate::app::PlanPermissions::default(),
            context_percent: None,
            intent: None,
            session_id: "sess-test".to_string(),
            message_index: 0,
            scratchpad: None,
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

                ollama_num_ctx: None,
                ollama_allow_ram_offload: None,
                resolved_context_window: None,
                resolved_max_output: None,
                output_schema: None,
                suppress_auto_compact: false,
            },
        });
        assert_eq!(r.scope_count(), 1);

        r.dispatch(Cmd::CancelScope(turn));
        assert_eq!(r.scope_count(), 0);
    }

    #[tokio::test]
    async fn tombstoned_turn_is_not_resurrected_by_late_scoped_cmd() {
        // F38: once a turn's scope has been cancelled (dropped + tombstoned), a
        // stray turn-scoped Cmd bearing the same TurnId must be dropped — not
        // used to spin up a fresh, un-cancelled scope via `scope_mut`'s
        // `or_insert_with`. Turn ids are monotonic and never reused, so such a
        // Cmd can only be a post-cancel straggler.
        let (mut r, _rx) = runner();
        let req = || crate::domain::ChatRequest {
            model_id: "test/m".to_string(),
            messages: vec![],
            system_prompt: String::new(),
            instructions: None,
            reasoning: crate::models::ReasoningLevel::Medium,
            temperature: 0.7,
            max_tokens: 4096,
            tools: vec![],
            ollama_num_ctx: None,
            ollama_allow_ram_offload: None,
            resolved_context_window: None,
            resolved_max_output: None,
            output_schema: None,
            suppress_auto_compact: false,
        };
        let turn = TurnId(123);

        r.dispatch(Cmd::CallModel {
            turn,
            request: req(),
        });
        assert_eq!(r.scope_count(), 1);

        // Cancel: drops the scope and tombstones the turn.
        r.dispatch(Cmd::CancelScope(turn));
        assert_eq!(r.scope_count(), 0);

        // A late scoped Cmd for the now-tombstoned turn must be dropped.
        r.dispatch(Cmd::CallModel {
            turn,
            request: req(),
        });
        assert_eq!(
            r.scope_count(),
            0,
            "a cancelled turn must not be resurrected by a late scoped Cmd"
        );

        // A fresh, higher turn id is unaffected by the tombstone.
        r.dispatch(Cmd::CallModel {
            turn: TurnId(124),
            request: req(),
        });
        assert_eq!(
            r.scope_count(),
            1,
            "a fresh turn must still create its scope normally"
        );
    }

    #[tokio::test]
    async fn shutdown_drains_pending_saves() {
        let (mut r, _rx) = runner();
        for _ in 0..5 {
            r.dispatch(Cmd::SaveConversation(
                crate::session::ConversationHistory::new(
                    "/p".to_string(),
                    "m".to_string(),
                    chrono::Local::now(),
                ),
            ));
        }
        // Shutdown waits for all five to complete (should be instant).
        let start = std::time::Instant::now();
        r.shutdown().await;
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    fn persistence_fixture(
        root: &std::path::Path,
        archive_id: &str,
    ) -> (crate::session::ConversationHistory, PendingCompactionSave) {
        let now = chrono::Local::now();
        let mut full = crate::session::ConversationHistory::new(
            root.display().to_string(),
            "test/model".to_string(),
            now,
        );
        full.add_messages(&[crate::models::ChatMessage::user("raw history")], now);
        let mut compacted = full.clone();
        compacted.replace_messages(
            vec![crate::models::ChatMessage::user("compacted checkpoint")],
            now,
        );
        let archive = crate::domain::CompactionArchive {
            id: archive_id.to_string(),
            conversation_id: full.id.clone(),
            created_at: now,
            messages: full.messages.clone(),
        };
        let record = crate::domain::CompactionRecord {
            id: archive_id.to_string(),
            trigger: crate::domain::CompactionTrigger::Manual,
            created_at: now,
            before_tokens: 100,
            after_tokens: 20,
            archived_message_count: 1,
            preserved_message_count: 1,
            preserved_turn_count: 1,
            summary_tokens: 10,
            duration_secs: 0.1,
            review_status: crate::domain::CompactionReviewStatus::Reviewed,
            review_error: None,
            focus: None,
            archive_path: None,
        };
        (
            full,
            PendingCompactionSave {
                archive,
                record,
                conversation: compacted,
                task_id: None,
            },
        )
    }

    #[test]
    fn persistence_orders_compaction_before_newer_conversation_save() {
        let root = std::env::temp_dir().join(format!(
            "mermaid-persistence-order-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let (full, compaction) = persistence_fixture(&root, "compact_ordered");
        let manager = crate::session::ConversationManager::new(&root).unwrap();
        manager.save_conversation(&full).unwrap();

        let mut state = PersistenceState::new(root.clone());
        let (events, outcome) =
            state.process(PersistenceJob::Compaction(Box::new(compaction.clone())));
        outcome.unwrap();
        assert_eq!(events.len(), 1);
        let mut newer = compaction.conversation;
        newer.add_messages(
            &[crate::models::ChatMessage::assistant("new assistant reply")],
            chrono::Local::now(),
        );
        let (_, outcome) = state.process(PersistenceJob::Conversation(Box::new(newer)));
        outcome.unwrap();

        let loaded = crate::session::ConversationManager::new(&root)
            .unwrap()
            .load_conversation(&full.id)
            .unwrap();
        assert!(
            loaded
                .messages
                .iter()
                .any(|message| message.content == "new assistant reply")
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn failed_archive_blocks_later_stripped_conversation_save() {
        let root = std::env::temp_dir().join(format!(
            "mermaid-persistence-barrier-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let (full, compaction) = persistence_fixture(&root, "../invalid");
        let manager = crate::session::ConversationManager::new(&root).unwrap();
        manager.save_conversation(&full).unwrap();

        let mut state = PersistenceState::new(root.clone());
        assert!(
            state
                .process(PersistenceJob::Compaction(Box::new(compaction.clone())))
                .1
                .is_err()
        );
        assert!(
            state
                .process(PersistenceJob::Conversation(Box::new(
                    compaction.conversation,
                )))
                .1
                .is_err()
        );
        assert_eq!(state.blocked.get(&full.id).map(VecDeque::len), Some(1));

        let loaded = crate::session::ConversationManager::new(&root)
            .unwrap()
            .load_conversation(&full.id)
            .unwrap();
        assert_eq!(loaded.messages[0].content, "raw history");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn blocked_barrier_queues_a_new_compaction_instead_of_dropping_it() {
        let root = std::env::temp_dir().join(format!(
            "mermaid-persistence-queue-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let (full, first) = persistence_fixture(&root, "../invalid");
        let mut second = first.clone();
        second.archive.id = "compact_second".to_string();
        second.record.id = "compact_second".to_string();

        let mut state = PersistenceState::new(root.clone());
        assert!(
            state
                .process(PersistenceJob::Compaction(Box::new(first)))
                .1
                .is_err()
        );
        // The older barrier still fails; the new save must queue behind it —
        // its archive is the only durable copy of the stripped messages.
        assert!(
            state
                .process(PersistenceJob::Compaction(Box::new(second)))
                .1
                .is_err()
        );
        let queued = state.blocked.get(&full.id).expect("barrier queue");
        assert_eq!(queued.len(), 2);
        assert_eq!(queued[0].archive.id, "../invalid");
        assert_eq!(queued[1].archive.id, "compact_second");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn retry_all_blocked_attempts_every_conversation() {
        let root = std::env::temp_dir().join(format!(
            "mermaid-persistence-drain-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let (bad_full, bad) = persistence_fixture(&root, "../invalid");
        let (mut good_full, mut good) = persistence_fixture(&root, "compact_good");
        // Conversation ids are millisecond timestamps; two fixtures minted in
        // the same instant would collide into one barrier queue. Force the
        // second conversation onto a distinct (still format-valid) id.
        good_full.id = "20990101_000000_001".to_string();
        good.archive.conversation_id = good_full.id.clone();
        good.conversation.id = good_full.id.clone();

        let mut state = PersistenceState::new(root.clone());
        state
            .blocked
            .entry(bad_full.id.clone())
            .or_default()
            .push_back(bad);
        state
            .blocked
            .entry(good_full.id.clone())
            .or_default()
            .push_back(good);

        // One conversation's bad disk state must not strand the other's
        // barrier at shutdown: the error surfaces, but the good save lands —
        // and its durably persisted event is reported alongside the error.
        let (events, outcome) = state.retry_all_blocked();
        assert!(outcome.is_err());
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, "compact_good");
        assert!(!state.blocked.contains_key(&good_full.id));
        assert_eq!(state.blocked.get(&bad_full.id).map(VecDeque::len), Some(1));
        let loaded = crate::session::ConversationManager::new(&root)
            .unwrap()
            .load_conversation(&good_full.id)
            .unwrap();
        assert_eq!(loaded.messages[0].content, "compacted checkpoint");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn partially_drained_barrier_reports_its_persisted_events() {
        let root = std::env::temp_dir().join(format!(
            "mermaid-persistence-partial-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let (full, good) = persistence_fixture(&root, "compact_good");
        let mut bad = good.clone();
        bad.archive.id = "../invalid".to_string();
        bad.record.id = "../invalid".to_string();

        let mut state = PersistenceState::new(root.clone());
        let queue = state.blocked.entry(full.id.clone()).or_default();
        queue.push_back(good);
        queue.push_back(bad);

        // The good save at the head of the queue persists durably before the
        // bad one fails. Its event must surface with the error — it is popped
        // and would otherwise never fire SessionSaved or the compaction hook.
        let (events, outcome) = state.retry_blocked(&full.id);
        assert!(outcome.is_err());
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, "compact_good");
        assert_eq!(state.blocked.get(&full.id).map(VecDeque::len), Some(1));
        let _ = std::fs::remove_dir_all(root);
    }
}
