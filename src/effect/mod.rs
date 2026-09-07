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
mod turn_scope;

use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use tokio::sync::mpsc;

use crate::providers::ctx::{ExecContext, StreamContext};
use crate::providers::{ProviderFactory, StreamEvent, ToolRegistry};
use mermaid_domain::{
    Cmd, CompactionRequest, CompactionResult, CompactionTrigger, Msg, Query, QueryResult, TurnId,
};
use mermaid_domain::{Config, MemoryConfig};

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

/// Translate a domain `CompactionEvent` into the durable row.
///
/// The two types share a name-adjacent concept and almost nothing else: the
/// domain event carries 14 fields describing what compaction did, the row
/// carries 9 describing what a later reader needs. Only `id` and
/// `archive_path` overlap. This lived as an anonymous struct literal inline in
/// `persist_compaction`, which is where a field mapping goes to rot unnoticed.
///
/// It stays in `effect`, not in the pure core: "domain value -> SQLite row" is
/// precisely what this layer is for, and the orphan rule permitting it
/// elsewhere is not a reason to put row-shaped knowledge in the reducer.
fn compaction_row(
    record: &mermaid_domain::CompactionEvent,
    archive_path: &std::path::Path,
    task_id: Option<String>,
    session_id: String,
) -> mermaid_runtime::NewCompaction {
    mermaid_runtime::NewCompaction {
        id: Some(record.id.clone()),
        task_id,
        session_id: Some(session_id),
        source_token_estimate: Some(record.before_tokens as i64),
        summary_token_count: Some(record.summary_tokens as i64),
        preserved_turns: Some(record.preserved_turn_count as i64),
        archive_path: Some(archive_path.display().to_string()),
        verification_status: Some(record.review_status.as_str().to_string()),
    }
}

/// Feed the cross-project session index off a successful snapshot save.
/// Best-effort by design: the row is an index over the files, never the
/// truth, so a store hiccup must not fail the save that just succeeded.
/// Feed the cross-project session index off a successful append. Best-effort
/// by design: the row is an index over the files, never the truth, so a store
/// hiccup must not fail the save that just succeeded; the daemon rebuilds the
/// index from disk on its next start regardless.
fn upsert_session_index(
    manager: &crate::session::ConversationManager,
    snapshot: &mermaid_domain::ConversationHistory,
) {
    let row = crate::session::session_row(manager.conversations_dir(), snapshot);
    let _ = mermaid_runtime::with_shared_store(|store| store.sessions().upsert(row));
}

#[derive(Clone)]
enum PersistenceJob {
    Conversation {
        snapshot: Box<mermaid_domain::ConversationHistory>,
        events: Vec<mermaid_domain::SessionEvent>,
    },
    Compaction(Box<PendingCompactionSave>),
}

#[derive(Clone)]
struct PendingCompactionSave {
    record: mermaid_domain::CompactionEvent,
    conversation: mermaid_domain::ConversationHistory,
    events: Vec<mermaid_domain::SessionEvent>,
    /// Set once the events are durably appended, so a retry after a later
    /// failure in the same save re-runs only what did not land — appending
    /// twice would duplicate the boundary in the log.
    events_appended: bool,
    task_id: Option<String>,
}

struct PersistedCompaction {
    id: String,
    task_id: Option<String>,
    session_id: String,
    archive_path: PathBuf,
}

/// How many appended events may accumulate before the checkpoint is
/// rewritten.
///
/// The quantity being bounded is REPLAY LENGTH — a checkpoint's whole job is
/// to cap how much log a resume has to fold — so the trigger counts events
/// rather than turns or seconds. At ~30 events for a tool-heavy turn this is
/// roughly seven turns of replay in the worst case, against a rewrite that
/// used to happen after every single message.
const CHECKPOINT_EVERY_EVENTS: usize = 200;

struct PersistenceState {
    workdir: PathBuf,
    manager: Option<crate::session::ConversationManager>,
    blocked: HashMap<String, VecDeque<PendingCompactionSave>>,
    /// Per session: events appended since its last checkpoint, and the
    /// newest snapshot that has not been written as one. Shutdown flushes
    /// these, so a clean exit always leaves a current checkpoint.
    dirty: HashMap<String, DirtySession>,
    /// Events whose append FAILED, kept in order for the next attempt.
    /// Without this they are simply gone: the reducer drains its buffer at
    /// emission, so a dropped batch is never re-offered — which stopped
    /// mattering the moment the log became the truth rather than a copy.
    unappended: HashMap<String, Vec<mermaid_domain::SessionEvent>>,
}

struct DirtySession {
    snapshot: mermaid_domain::ConversationHistory,
    events_since_checkpoint: usize,
}

impl PersistenceState {
    fn new(workdir: PathBuf) -> Self {
        Self {
            workdir,
            manager: None,
            blocked: HashMap::new(),
            dirty: HashMap::new(),
            unappended: HashMap::new(),
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
            PersistenceJob::Conversation { snapshot, events } => {
                // Barrier: a still-blocked compaction must persist before any
                // newer (stripped) conversation snapshot may overwrite the file.
                let (persisted, retried) = self.retry_blocked(&snapshot.id);
                if retried.is_err() {
                    return (persisted, retried);
                }
                let saved = self.save_session(*snapshot, events);
                (persisted, saved)
            },
            PersistenceJob::Compaction(save) => {
                // Queue first, then drain. The boundary event is the only
                // record that the dropped messages ever existed, so the save
                // must survive an Err AND a panic in the write path (pop
                // happens only after success), and it must land behind any
                // older still-blocked saves (FIFO).
                let conversation_id = save.conversation.id.clone();
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
        let dirty = &mut self.dirty;
        let queue = self
            .blocked
            .get_mut(conversation_id)
            .expect("checked above");
        while let Some(save) = queue.front_mut() {
            match Self::persist_compaction(manager, dirty, save) {
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

    /// Append a save's events, then write the checkpoint only when enough
    /// have accumulated (see [`CHECKPOINT_EVERY_EVENTS`]).
    ///
    /// The append is the save now: it is the write that reaches the truth,
    /// so a failure keeps the batch for the next attempt rather than
    /// dropping it, and leaves the checkpoint alone — advancing a cache past
    /// a log that did not take the events is how a "successful" save loses
    /// them.
    fn save_session(
        &mut self,
        snapshot: mermaid_domain::ConversationHistory,
        events: Vec<mermaid_domain::SessionEvent>,
    ) -> anyhow::Result<()> {
        let id = snapshot.id.clone();
        // Anything a previous attempt could not land goes first, in order.
        let mut batch = self.unappended.remove(&id).unwrap_or_default();
        batch.extend(events);

        let manager = self.manager()?;
        if let Err(error) = manager.append_session_events(&snapshot, &batch) {
            tracing::warn!(
                id = %id,
                pending = batch.len(),
                %error,
                "session event append failed; holding the events for the next save"
            );
            self.unappended.insert(id, batch);
            return Err(error);
        }
        upsert_session_index(manager, &snapshot);

        let entry = self
            .dirty
            .entry(id.clone())
            .or_insert_with(|| DirtySession {
                snapshot: snapshot.clone(),
                events_since_checkpoint: 0,
            });
        entry.snapshot = snapshot;
        entry.events_since_checkpoint += batch.len();
        if entry.events_since_checkpoint >= CHECKPOINT_EVERY_EVENTS {
            return self.write_checkpoint(&id);
        }
        Ok(())
    }

    /// Materialize a session's checkpoint and clear its dirty counter. A
    /// session with nothing outstanding is a no-op.
    fn write_checkpoint(&mut self, id: &str) -> anyhow::Result<()> {
        let Some(dirty) = self.dirty.remove(id) else {
            return Ok(());
        };
        let manager = self.manager()?;
        if let Err(error) = manager.save_conversation(&dirty.snapshot) {
            // Put it back: the events are durable in the log either way, so
            // this costs a longer replay, not data — but the next save
            // should still try.
            self.dirty.insert(id.to_string(), dirty);
            return Err(error);
        }
        Ok(())
    }

    /// Write every outstanding checkpoint. Called at shutdown so a clean
    /// exit always leaves a current one, which is what keeps the common
    /// resume path short.
    fn flush_checkpoints(&mut self) -> anyhow::Result<()> {
        let ids: Vec<String> = self.dirty.keys().cloned().collect();
        let mut first_error = None;
        for id in ids {
            if let Err(error) = self.write_checkpoint(&id) {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
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
        dirty: &mut HashMap<String, DirtySession>,
        save: &mut PendingCompactionSave,
    ) -> anyhow::Result<PersistedCompaction> {
        // Order is the whole guarantee. A compaction drops messages from the
        // conversation, and the only remaining record of them is this
        // session's log — the earlier `message` events plus the `compaction`
        // event that marks the boundary. So the append must land BEFORE the
        // stripped snapshot overwrites the file that still holds the fuller
        // history, and a failed append must abort the save (this is where
        // the archive file's `?` used to be), leaving the queue blocked and
        // the old snapshot intact for the retry.
        if !save.events_appended {
            manager.append_session_events(&save.conversation, &save.events)?;
            save.events_appended = true;
        }
        manager.save_conversation(&save.conversation)?;
        upsert_session_index(manager, &save.conversation);
        // A compaction always checkpoints: it is a structural boundary, it
        // is rare, and the transcript it leaves behind is the one a resume
        // should start from rather than replay its way to.
        dirty.remove(&save.conversation.id);

        let log_path = manager.event_log_path(&save.conversation.id);
        let _ = mermaid_runtime::with_shared_store(|store| {
            store.compactions().create(compaction_row(
                &save.record,
                &log_path,
                save.task_id.clone(),
                save.conversation.id.clone(),
            ))
        });

        Ok(PersistedCompaction {
            id: save.record.id.clone(),
            task_id: save.task_id.clone(),
            session_id: save.conversation.id.clone(),
            archive_path: log_path,
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
    /// `TurnId` creates a scope; `Cmd::CancelScope` tears it down.
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
    #[must_use]
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

    /// The channel every effect result arrives on.
    ///
    /// Cloning it is how the brokers already deliver a user's approval or
    /// answer back into the reducer; `crate::engine::EngineHandle` uses it to
    /// give the same reach to something outside the process's own effects.
    #[must_use]
    pub fn sender(&self) -> MsgSender {
        self.msg_tx.clone()
    }

    /// Enable inline approval prompts (interactive TUI only). The gate then
    /// pauses gated tools and routes the user's decision through the
    /// `ApprovalBroker` instead of writing an out-of-band DB approval row.
    #[must_use]
    pub fn with_interactive_approvals(mut self) -> Self {
        self.approval = Some(crate::providers::ApprovalBroker::new(self.msg_tx.clone()));
        self
    }

    /// Enable inline `ask_user_question` prompts (interactive TUI only). The tool
    /// then parks on the `QuestionBroker` and routes the user's answers back
    /// through it instead of proceeding without asking.
    #[must_use]
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
    #[must_use]
    pub fn with_task_id(mut self, task_id: Option<String>) -> Self {
        self.task_id = task_id;
        self
    }

    /// Disable terminal-title writes for non-interactive callers.
    #[must_use]
    pub fn without_terminal_title(mut self) -> Self {
        self.terminal_title_enabled = false;
        self
    }

    /// Leave the process-global MCP manager alone on `shutdown`. Child
    /// (subagent) runners share it with the parent and must not reap it.
    #[must_use]
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
    #[must_use]
    pub fn pair(workdir: PathBuf) -> (Self, mpsc::Receiver<Msg>) {
        let (tx, rx) = mpsc::channel(MSG_CHANNEL_CAPACITY);
        (Self::new(tx, workdir), rx)
    }

    /// Pair constructor that also wires the real provider factory +
    /// tool registry. Used by `app::run_interactive`.
    #[must_use]
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

    /// Run one read-only `Cmd::Query` lookup and answer with
    /// `Msg::QueryResult`. Conversation reads and provider discovery run
    /// async; everything touching the runtime store or the filesystem walk
    /// goes through [`Self::send_blocking_query`] so a synchronous read never
    /// stalls an async worker thread (#40).
    fn dispatch_query(&mut self, query: Query) {
        let tx = self.msg_tx.clone();
        match query {
            Query::LoadConversation { id } => self.query_load_conversation(id, tx),
            Query::ListConversations => self.query_list_conversations(tx),
            Query::ListAvailableModels => {
                let providers = self.providers.clone();
                self.detached.spawn(async move {
                    let choices = discover_available_models(providers).await;
                    let _ = tx
                        .send(Msg::QueryResult(QueryResult::AvailableModelsListed(
                            choices,
                        )))
                        .await;
                });
            },
            Query::ListProjectFiles => {
                let workdir = self.workdir.clone();
                self.send_blocking_query(move || {
                    QueryResult::ProjectFilesListed(walk_project_files(&workdir))
                });
            },
            Query::ListRuntimeTasks { limit } => self.send_blocking_query(move || {
                QueryResult::RuntimeTasksListed(
                    crate::runtime_client::RuntimeClient::auto()
                        .list_tasks(limit)
                        .map(|read| read.value)
                        .unwrap_or_default(),
                )
            }),
            Query::LoadRuntimeTask { id } => self.send_blocking_query(move || {
                let (task, events) = crate::runtime_client::RuntimeClient::auto()
                    .task_detail(&id)
                    .map(|read| (Some(Box::new(read.value.task)), read.value.events))
                    .unwrap_or((None, Vec::new()));
                QueryResult::RuntimeTaskLoaded { task, events }
            }),
            Query::ListRuntimeProcesses { limit } => self.send_blocking_query(move || {
                QueryResult::RuntimeProcessesListed(
                    crate::runtime_client::RuntimeClient::auto()
                        .list_processes(limit)
                        .map(|read| read.value)
                        .unwrap_or_default(),
                )
            }),
            Query::ListRuntimeApprovals => self.send_blocking_query(move || {
                QueryResult::RuntimeApprovalsListed(
                    crate::runtime_client::RuntimeClient::auto()
                        .list_approvals()
                        .map(|read| read.value)
                        .unwrap_or_default(),
                )
            }),
            Query::ListRuntimeCheckpoints { limit } => self.send_blocking_query(move || {
                QueryResult::RuntimeCheckpointsListed(
                    crate::runtime_client::RuntimeClient::auto()
                        .list_checkpoints(limit)
                        .map(|read| read.value)
                        .unwrap_or_default(),
                )
            }),
            Query::ListForkCheckpoints {
                session_id,
                message_index,
            } => self.send_blocking_query(move || {
                QueryResult::ForkCheckpointsFound(
                    mermaid_runtime::with_shared_store(|store| {
                        store
                            .checkpoints()
                            .list_for_session(&session_id, message_index as i64)
                    })
                    .unwrap_or_default(),
                )
            }),
            Query::ListRuntimePlugins => self.send_blocking_query(move || {
                QueryResult::RuntimePluginsListed(
                    crate::runtime_client::RuntimeClient::auto()
                        .list_plugins()
                        .map(|read| read.value)
                        .unwrap_or_default(),
                )
            }),
        }
    }

    /// `Query::LoadConversation` — read one saved conversation off disk. A
    /// missing/corrupt file answers nothing: the failure is logged and the
    /// picker simply does not advance.
    fn query_load_conversation(&mut self, id: String, tx: MsgSender) {
        let workdir = self.workdir.clone();
        self.detached.spawn(async move {
            match crate::session::ConversationManager::new(&workdir) {
                Ok(mgr) => match mgr.load_conversation(&id) {
                    Ok(history) => {
                        let _ = tx
                            .send(Msg::QueryResult(QueryResult::ConversationLoaded(Box::new(
                                history,
                            ))))
                            .await;
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
    }

    /// `Query::ListConversations` — scan the conversations directory for the
    /// `/load` picker (newest first).
    fn query_list_conversations(&mut self, tx: MsgSender) {
        let workdir = self.workdir.clone();
        self.detached.spawn(async move {
            let summaries = match crate::session::ConversationManager::new(&workdir) {
                Ok(mgr) => mgr
                    .list_conversation_metas()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|m| mermaid_domain::ConversationSummary {
                        id: m.id,
                        title: m.title,
                        message_count: m.message_count,
                        updated_at: m.updated_at.to_rfc3339(),
                    })
                    .collect(),
                Err(_) => Vec::new(),
            };
            let _ = tx
                .send(Msg::QueryResult(QueryResult::ConversationsListed(
                    summaries,
                )))
                .await;
        });
    }

    /// Run a synchronous lookup on the blocking pool and deliver its
    /// `Msg::QueryResult` — the shared plumbing of every store/filesystem
    /// query (rusqlite reads and the project walk must never stall an async
    /// worker thread, #40).
    fn send_blocking_query(&mut self, run: impl FnOnce() -> QueryResult + Send + 'static) {
        let tx = self.msg_tx.clone();
        self.detached.spawn_blocking(move || {
            let _ = tx.blocking_send(Msg::QueryResult(run()));
        });
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
    #[must_use]
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

    /// Harvest finished detached tasks. Without this the `detached` `JoinSet`
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
    #[expect(
        clippy::too_many_lines,
        reason = "the effect router: one arm per Cmd variant, each spawning or calling the \
         handler that owns that effect; the arms are short and the routing table is the point, so \
         the length is the Cmd count and drops only as commands are retired"
    )]
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
                    let mut enriched =
                        filter_suppressed(tools.describe_all(), &request.suppressed_builtin_tools);
                    // Report the built-in tool-schema token cost so the
                    // reducer's /context preview can fold it into its MCP-only
                    // estimate and agree with what the model actually sees.
                    // Runs AFTER suppression so the estimate matches reality.
                    let builtin_tokens = mermaid_domain::estimate_tool_schema_tokens(&enriched);
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
                                error: mermaid_model::models::UserFacingError {
                                    summary: "Internal error".to_string(),
                                    message: "The model dispatch task panicked unexpectedly."
                                        .to_string(),
                                    suggestion: "This is a bug. Please retry; if it persists, \
                                                 check the logs."
                                        .to_string(),
                                    category: mermaid_model::models::ErrorCategory::Internal,
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
                                kind: mermaid_domain::StatusKind::Error,
                            })
                            .await;
                    }
                });
            },
            Cmd::ExecuteTool {
                turn,
                call_id,
                source,
                dispatch,
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
                    .unwrap_or_else(|| Arc::new(mermaid_domain::Config::default()));
                // Auto mode: build an LLM classifier to vet borderline
                // actions. Only when a provider is bound (real wiring); the
                // gate fails safe to "escalate" when it's `None`. The vet
                // uses the configured classifier model, else the session model.
                // Plan mode also gets one: profile levels set to `auto`
                // resolve through `PolicyDecision::Classify`, which fails
                // safe to escalate without a classifier bound.
                let classifier: Option<Arc<dyn crate::providers::AutoClassifier>> =
                    if dispatch.safety_mode == mermaid_runtime::SafetyMode::Auto
                        || dispatch.plan_file.is_some()
                    {
                        self.providers.as_ref().map(|p| {
                            let model = config
                                .safety
                                .auto_classifier_model
                                .clone()
                                .unwrap_or_else(|| dispatch.model_id.clone());
                            Arc::new(crate::providers::ModelAutoClassifier::new(p.clone(), model))
                                as Arc<dyn crate::providers::AutoClassifier>
                        })
                    } else {
                        None
                    };
                let services = crate::providers::ctx::ToolServices {
                    workdir,
                    config,
                    task_id: self.task_id.clone(),
                    // Detached work (backgrounded subagents) reports back
                    // through the main msg channel after this turn's
                    // progress relay is gone.
                    notify: Some(self.msg_tx.clone()),
                    classifier,
                    approval: self.approval.clone(),
                    questions: self.questions.clone(),
                    tasks: Some(self.tasks.clone()),
                };
                let scope = self.scope_mut(turn);
                let signals = crate::providers::ctx::TurnSignals {
                    token: scope.token(),
                    background: scope.background_token(),
                    web_bytes: scope.web_bytes(),
                };
                scope.spawn(async move {
                    use futures::FutureExt;
                    let fallback_tx = tx.clone();
                    if std::panic::AssertUnwindSafe(dispatch_execute_tool(
                        tx, tools, turn, call_id, source, signals, dispatch, services,
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
                                outcome: mermaid_domain::ToolOutcome::error(
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
                    let reason = mermaid_model::utils::redact_secrets(&reason);
                    let _ = broker
                        .update(vec![mermaid_domain::ChecklistEdit {
                            id: task.id,
                            status: Some(mermaid_domain::ChecklistStatus::InProgress),
                            ..mermaid_domain::ChecklistEdit::default()
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
            Cmd::SaveConversation { snapshot, events } => {
                self.queue_persistence(PersistenceJob::Conversation {
                    snapshot: Box::new(snapshot),
                    events,
                });
            },
            Cmd::SaveCompaction {
                record,
                conversation,
                events,
            } => {
                self.queue_persistence(PersistenceJob::Compaction(Box::new(
                    PendingCompactionSave {
                        record,
                        conversation,
                        events,
                        events_appended: false,
                        task_id: self.task_id.clone(),
                    },
                )));
            },
            Cmd::SaveProcess(process) => {
                let task_id = self.task_id.clone();
                self.detached.spawn(async move {
                    let status = process.status;
                    let _ = mermaid_runtime::with_shared_store(|store| {
                        store.processes().upsert(mermaid_runtime::NewProcess {
                            id: Some(process.id),
                            task_id,
                            pid: process.pid,
                            command: process.command,
                            cwd: process.cwd,
                            log_path: Some(process.log_path),
                            detected_url: process.detected_url,
                            status,
                            health: None,
                        })
                    });
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
                        mermaid_domain::MemoryScope::ProjectPrivate,
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
            Cmd::Query(query) => self.dispatch_query(query),
            Cmd::ShowRuntimeProcessLogs { id } => {
                let tx = self.msg_tx.clone();
                self.detached.spawn_blocking(move || {
                    let text = crate::runtime_client::RuntimeClient::auto()
                        .process_log(&id, None)
                        .map(|log| format!("Process log {}\n\n{}", id, log.content))
                        .unwrap_or_else(|err| format!("Process log error: {err}"));
                    let _ = tx.blocking_send(Msg::RuntimeText(text));
                });
            },
            Cmd::StopRuntimeProcess { id } => {
                let tx = self.msg_tx.clone();
                self.detached.spawn_blocking(move || {
                    let msg = match crate::runtime_client::RuntimeClient::auto().stop_process(&id) {
                        Ok(response) => Msg::TransientStatus {
                            text: format!("Stopped process {} (pid {})", id, response.item.pid),
                        },
                        Err(err) => Msg::TransientStatus {
                            text: format!("Process stop failed: {err}"),
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
                            // Killing a finished child hands back the
                            // workspace it was holding for a continuation
                            // that can no longer happen. Discarding it needs
                            // to be async, so it rides a task.
                            if let crate::providers::tool::subagent::KillResult::Evicted(
                                workspace,
                            ) = spawner.kill_detached(&id)
                            {
                                // In `detached`, not a bare `tokio::spawn`: shutdown
                                // drains that set, so quitting mid-discard cannot
                                // leave the worktree on disk with no record of it.
                                self.detached.spawn(async move {
                                    workspace.discard().await;
                                });
                            }
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
                    let msg = match crate::runtime_client::RuntimeClient::auto()
                        .restart_process(&id)
                    {
                        Ok(response) => Msg::TransientStatus {
                            text: format!("Restarted process {} (pid {})", id, response.item.pid),
                        },
                        Err(err) => Msg::TransientStatus {
                            text: format!("Process restart failed: {err}"),
                        },
                    };
                    let _ = tx.blocking_send(msg);
                });
            },
            Cmd::OpenRuntimeTarget { target } => {
                self.detached.spawn_blocking(move || {
                    let resolved = crate::runtime_client::RuntimeService::open_default()
                        .and_then(|service| service.resolve_open_target(&target))
                        .unwrap_or(target);
                    // #63: the resolved value can be a `detected_url`/`log_path`
                    // from a `processes` row — validate before the OS opener,
                    // exactly like `open_process`.
                    if let Err(err) = crate::runtime_client::validate_open_target(&resolved) {
                        tracing::warn!(error = %err, "refusing to open runtime target");
                        return;
                    }
                    mermaid_model::utils::open_file(resolved);
                });
            },
            Cmd::ShowRuntimePorts => {
                let tx = self.msg_tx.clone();
                self.detached.spawn_blocking(move || {
                    let text = crate::runtime_client::RuntimeClient::auto()
                        .ports()
                        .map(|ports| format!("Listening TCP ports\n\n{}", ports.ports))
                        .unwrap_or_else(|err| format!("Port inspection failed: {err}"));
                    let _ = tx.blocking_send(Msg::RuntimeText(text));
                });
            },
            Cmd::DecideRuntimeApproval { id, decision } => {
                let tx = self.msg_tx.clone();
                self.detached.spawn_blocking(move || {
                    let result = if decision == "approved" {
                        crate::runtime_client::RuntimeClient::auto().approve(&id)
                    } else {
                        crate::runtime_client::RuntimeClient::auto().deny(&id)
                    };
                    let msg = match result {
                        Ok(result) => Msg::TransientStatus {
                            text: if result.replayed {
                                format!("Approval {} {}: {}", id, decision, result.summary)
                            } else {
                                format!("Approval {id} {decision}")
                            },
                        },
                        Err(err) => Msg::TransientStatus {
                            text: format!("Approval update failed: {err}"),
                        },
                    };
                    let _ = tx.blocking_send(msg);
                });
            },
            Cmd::UpdateRuntimeTaskStatus {
                id,
                status,
                final_report,
            } => {
                let tx = self.msg_tx.clone();
                self.detached.spawn_blocking(move || {
                    let msg = match mermaid_runtime::with_shared_store(|store| {
                        store
                            .tasks()
                            .update_status(&id, status, final_report.as_deref())
                    }) {
                        Ok(()) => Msg::TransientStatus {
                            text: format!("Task {id} -> {status}"),
                        },
                        Err(err) => Msg::TransientStatus {
                            text: format!("Task update failed: {err}"),
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
                    let msg = match mermaid_runtime::create_checkpoint(
                        &workdir,
                        &paths,
                        pending_action,
                    ) {
                        Ok(manifest) => Msg::TransientStatus {
                            text: format!(
                                "Checkpoint {} created for {} path(s)",
                                manifest.id,
                                manifest.files.len()
                            ),
                        },
                        Err(err) => Msg::TransientStatus {
                            text: format!("Checkpoint failed: {err}"),
                        },
                    };
                    let _ = tx.blocking_send(msg);
                });
            },
            Cmd::RestoreRuntimeCheckpoint { id } => {
                let tx = self.msg_tx.clone();
                self.detached.spawn_blocking(move || {
                    let msg = match crate::runtime_client::RuntimeClient::auto()
                        .restore_checkpoint(&id)
                    {
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
                            text: format!("Restore failed: {err}"),
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
                        mermaid_model::utils::open_file(&path);
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
                    let seq = format!("\x1b]2;{title}\x07");
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
                let mut state = persistence_state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                let drained = state.retry_all_blocked();
                // Then materialize whatever the ~200-event throttle has been
                // holding back, so a clean exit always leaves a current
                // checkpoint and the next resume replays nothing.
                if let Err(error) = state.flush_checkpoints() {
                    tracing::warn!(
                        error = %error,
                        "shutdown: could not flush a session checkpoint; the log still has everything, so the next resume just folds further"
                    );
                }
                drop(state);
                drained
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
                // Tear down the auto-managed SearXNG process (zero-config
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
/// between `in_progress` and completed stamps.
fn note_stream_usage(
    tasks: &crate::providers::TaskBroker,
    usage: &Option<mermaid_model::models::TokenUsage>,
) {
    if let Some(usage) = usage {
        tasks.add_tokens(usage.completion_tokens as u64);
    }
}

/// Drop the built-in tool definitions the reducer suppressed for this request
/// (`ChatRequest::suppressed_builtin_tools` — e.g. the task-checklist writers
/// while a plan is being drafted). Pure so it unit-tests without the runner.
fn filter_suppressed(
    tools: Vec<mermaid_domain::ToolDefinition>,
    suppressed: &[&'static str],
) -> Vec<mermaid_domain::ToolDefinition> {
    if suppressed.is_empty() {
        return tools;
    }
    tools
        .into_iter()
        .filter(|t| !suppressed.contains(&t.name.as_str()))
        .collect()
}

mod compaction;
mod memory;
mod model_call;
mod tool_call;

use compaction::*;
use memory::*;
use model_call::*;
use tool_call::*;

#[cfg(test)]
mod tests {
    /// Pins the domain-event -> durable-row field mapping. Only `id` and
    /// `archive_path` share a name between the two types, so every other line
    /// of `compaction_row` is a decision nothing else records.
    #[test]
    fn compaction_row_maps_every_field_it_claims_to() {
        use mermaid_domain::{CompactionEvent, CompactionReviewStatus, CompactionTrigger};
        let record = CompactionEvent {
            id: "cmp-1".to_string(),
            trigger: CompactionTrigger::Manual,
            created_at: chrono::Local::now(),
            before_tokens: 9_000,
            after_tokens: 1_200,
            archived_message_count: 40,
            preserved_message_count: 6,
            preserved_turn_count: 3,
            summary_tokens: 450,
            duration_secs: 1.5,
            review_status: CompactionReviewStatus::Reviewed,
            review_error: None,
            focus: None,
            archive_path: None,
        };
        let row = compaction_row(
            &record,
            std::path::Path::new("/tmp/archive.json"),
            Some("task-7".to_string()),
            "sess-3".to_string(),
        );
        assert_eq!(row.id.as_deref(), Some("cmp-1"));
        assert_eq!(row.task_id.as_deref(), Some("task-7"));
        assert_eq!(row.session_id.as_deref(), Some("sess-3"));
        assert_eq!(row.source_token_estimate, Some(9_000));
        assert_eq!(row.summary_token_count, Some(450));
        assert_eq!(row.preserved_turns, Some(3));
        assert!(row.archive_path.is_some_and(|p| p.contains("archive.json")));
        assert_eq!(
            row.verification_status.as_deref(),
            Some(CompactionReviewStatus::Reviewed.as_str())
        );
    }

    use super::*;
    use mermaid_domain::ToolCallId;
    use std::time::Duration;

    fn runner() -> (EffectRunner, mpsc::Receiver<Msg>) {
        EffectRunner::pair(PathBuf::from("/tmp"))
    }

    /// The reducer's `suppressed_builtin_tools` contract: named tools drop
    /// out of the advertised set, everything else passes through in order.
    #[test]
    fn filter_suppressed_drops_only_the_named_tools() {
        let def = |name: &str| mermaid_domain::ToolDefinition {
            name: name.to_string(),
            description: String::new(),
            input_schema: serde_json::json!({}),
        };
        let tools = vec![def("task_create"), def("task_list"), def("task_update")];
        let kept = filter_suppressed(tools.clone(), &["task_create", "task_update"]);
        assert_eq!(
            kept.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            vec!["task_list"]
        );
        let kept = filter_suppressed(tools, &[]);
        assert_eq!(kept.len(), 3, "empty suppression list is a no-op");
    }

    #[test]
    fn runtime_tool_payloads_are_redacted_before_serialization() {
        let payload = serde_json::json!({
            "url": "https://user:hunter2@example.test/page?X-Amz-Signature=opaque-signature#private",
            "authorization": "opaque-secret-value",
            "model_content": "Fetched page says OPENAI_API_KEY=sk-abcdefghijklmnop1234 and Authorization: Bearer abcdef123456ghijkl",
        });
        let serialized = redacted_json_string(&payload).expect("serialize redacted payload");
        assert!(
            !serialized.contains("hunter2"),
            "URL password leaked: {serialized}"
        );
        assert!(
            !serialized.contains("opaque-signature"),
            "signed URL leaked: {serialized}"
        );
        assert!(
            !serialized.contains("private"),
            "URL fragment leaked: {serialized}"
        );
        assert!(
            !serialized.contains("opaque-secret-value"),
            "credential-named field leaked: {serialized}"
        );
        assert!(
            !serialized.contains("abcdef123456ghijkl"),
            "bearer token leaked: {serialized}"
        );
        assert!(
            !serialized.contains("sk-abcdefghijklmnop1234"),
            "secret-shaped fetched content leaked: {serialized}"
        );
        assert!(serialized.contains("[REDACTED]"));
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
        let providers = Arc::new(ProviderFactory::new(mermaid_domain::Config::default()));
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
        let providers = Arc::new(ProviderFactory::new(mermaid_domain::Config::default()));
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
        r.dispatch(Cmd::SaveConversation {
            snapshot: mermaid_domain::ConversationHistory::new(
                "/p".to_string(),
                "m".to_string(),
                chrono::Local::now(),
            ),
            events: Vec::new(),
        });
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
                mermaid_domain::McpServerConfig {
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
        let request = mermaid_domain::ChatRequest {
            model_id: "test/m".to_string(),
            messages: vec![],
            system_prompt: String::new(),
            instructions: None,
            reasoning: mermaid_model::models::ReasoningLevel::Medium,
            temperature: 0.7,
            max_tokens: 4096,
            tools: vec![],

            ollama_num_ctx: None,
            ollama_allow_ram_offload: None,
            resolved_context_window: None,
            resolved_max_output: None,
            output_schema: None,
            suppress_auto_compact: false,
            suppressed_builtin_tools: Vec::new(),
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
        let request = mermaid_domain::ChatRequest {
            model_id: "test/m".to_string(),
            messages: vec![],
            system_prompt: String::new(),
            instructions: None,
            reasoning: mermaid_model::models::ReasoningLevel::Medium,
            temperature: 0.7,
            max_tokens: 4096,
            tools: vec![],

            ollama_num_ctx: None,
            ollama_allow_ram_offload: None,
            resolved_context_window: None,
            resolved_max_output: None,
            output_schema: None,
            suppress_auto_compact: false,
            suppressed_builtin_tools: Vec::new(),
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
        let source = mermaid_model::models::tool_call::ToolCall {
            id: Some("c1".to_string()),
            function: mermaid_model::models::tool_call::FunctionCall {
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "x"}),
            },
        };
        r.dispatch(Cmd::ExecuteTool {
            turn,
            call_id,
            source,
            dispatch: mermaid_domain::ToolDispatch {
                model_id: "ollama/test".to_string(),
                safety_mode: mermaid_runtime::SafetyMode::Ask,
                plan_file: None,
                plan_permissions: mermaid_domain::PlanPermissions::default(),
                context_percent: None,
                intent: None,
                session_id: "sess-test".to_string(),
                message_index: 0,
                scratchpad: None,
            },
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
            request: mermaid_domain::ChatRequest {
                model_id: "m".to_string(),
                messages: vec![],
                system_prompt: String::new(),
                instructions: None,
                reasoning: mermaid_model::models::ReasoningLevel::Medium,
                temperature: 0.7,
                max_tokens: 4096,
                tools: vec![],

                ollama_num_ctx: None,
                ollama_allow_ram_offload: None,
                resolved_context_window: None,
                resolved_max_output: None,
                output_schema: None,
                suppress_auto_compact: false,
                suppressed_builtin_tools: Vec::new(),
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
        let req = || mermaid_domain::ChatRequest {
            model_id: "test/m".to_string(),
            messages: vec![],
            system_prompt: String::new(),
            instructions: None,
            reasoning: mermaid_model::models::ReasoningLevel::Medium,
            temperature: 0.7,
            max_tokens: 4096,
            tools: vec![],
            ollama_num_ctx: None,
            ollama_allow_ram_offload: None,
            resolved_context_window: None,
            resolved_max_output: None,
            output_schema: None,
            suppress_auto_compact: false,
            suppressed_builtin_tools: Vec::new(),
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
            r.dispatch(Cmd::SaveConversation {
                snapshot: mermaid_domain::ConversationHistory::new(
                    "/p".to_string(),
                    "m".to_string(),
                    chrono::Local::now(),
                ),
                events: Vec::new(),
            });
        }
        // Shutdown waits for all five to complete (should be instant).
        let start = std::time::Instant::now();
        r.shutdown().await;
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    fn persistence_fixture(
        root: &std::path::Path,
        record_id: &str,
    ) -> (mermaid_domain::ConversationHistory, PendingCompactionSave) {
        let now = chrono::Local::now();
        let mut full = mermaid_domain::ConversationHistory::new(
            root.display().to_string(),
            "test/model".to_string(),
            now,
        );
        full.add_messages(
            &[mermaid_model::models::ChatMessage::user("raw history")],
            now,
        );
        let mut compacted = full.clone();
        compacted.replace_messages(
            vec![mermaid_model::models::ChatMessage::user(
                "compacted checkpoint",
            )],
            now,
        );
        let record = mermaid_domain::CompactionEvent {
            id: record_id.to_string(),
            trigger: mermaid_domain::CompactionTrigger::Manual,
            created_at: now,
            before_tokens: 100,
            after_tokens: 20,
            archived_message_count: 1,
            preserved_message_count: 1,
            preserved_turn_count: 1,
            summary_tokens: 10,
            duration_secs: 0.1,
            review_status: mermaid_domain::CompactionReviewStatus::Reviewed,
            review_error: None,
            focus: None,
            archive_path: None,
        };
        (
            full,
            PendingCompactionSave {
                record,
                conversation: compacted,
                // The boundary event the save must land before it overwrites
                // the snapshot.
                events: vec![mermaid_domain::SessionEvent::Input {
                    text: "compaction boundary".to_string(),
                }],
                events_appended: false,
                task_id: None,
            },
        )
    }

    /// Make every event append for `id` fail, by planting a directory where
    /// its log file goes. This is the failure the barrier exists for now
    /// that the boundary event -- not an archive file -- is the only record
    /// of a compaction's dropped messages.
    fn block_event_log(root: &std::path::Path, id: &str) {
        let dir = root.join(".mermaid").join("conversations");
        std::fs::create_dir_all(&dir).expect("conversations dir");
        std::fs::create_dir_all(dir.join(format!("{id}.jsonl"))).expect("plant a blocker");
    }

    /// One `Message` event plus the snapshot that now contains it.
    fn one_message_save(
        conversation: &mut mermaid_domain::ConversationHistory,
        text: &str,
    ) -> PersistenceJob {
        let message = mermaid_model::models::ChatMessage::user(text);
        conversation.add_messages(std::slice::from_ref(&message), chrono::Local::now());
        PersistenceJob::Conversation {
            snapshot: Box::new(conversation.clone()),
            events: vec![mermaid_domain::SessionEvent::Message { message }],
        }
    }

    #[test]
    fn the_checkpoint_stops_being_written_on_every_save() {
        // The point of the throttle: appends stay O(1) per message while
        // the whole-transcript rewrite happens on a coarse cadence. What
        // must NOT change is what a resume sees.
        let root = std::env::temp_dir().join(format!(
            "mermaid-throttle-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let manager = crate::session::ConversationManager::new(&root).unwrap();
        let mut conversation = mermaid_domain::ConversationHistory::new(
            root.display().to_string(),
            "test/model".to_string(),
            chrono::Local::now(),
        );
        let mut state = PersistenceState::new(root.clone());

        // The first save creates the log (and its backfill) but no
        // checkpoint: nowhere near the threshold.
        state
            .process(one_message_save(&mut conversation, "first"))
            .1
            .unwrap();
        let checkpoint = manager
            .conversations_dir()
            .join(format!("{}.json", conversation.id));
        assert!(
            !checkpoint.exists(),
            "a single save must not rewrite the transcript"
        );

        // ...and resume still sees it, because the log is the truth.
        let resumed = manager.load_conversation(&conversation.id).unwrap();
        assert_eq!(resumed.messages().len(), 1);
        assert_eq!(resumed.messages()[0].content, "first");

        // Crossing the threshold materializes one.
        for i in 0..CHECKPOINT_EVERY_EVENTS {
            state
                .process(one_message_save(&mut conversation, &format!("m{i}")))
                .1
                .unwrap();
        }
        assert!(
            checkpoint.exists(),
            "crossing {CHECKPOINT_EVERY_EVENTS} events must materialize a checkpoint"
        );
        let resumed = manager.load_conversation(&conversation.id).unwrap();
        assert_eq!(resumed.messages().len(), CHECKPOINT_EVERY_EVENTS + 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn shutdown_flushes_the_checkpoint_it_was_holding() {
        let root = std::env::temp_dir().join(format!(
            "mermaid-flush-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let manager = crate::session::ConversationManager::new(&root).unwrap();
        let mut conversation = mermaid_domain::ConversationHistory::new(
            root.display().to_string(),
            "test/model".to_string(),
            chrono::Local::now(),
        );
        let mut state = PersistenceState::new(root.clone());
        state
            .process(one_message_save(&mut conversation, "only message"))
            .1
            .unwrap();

        let checkpoint = manager
            .conversations_dir()
            .join(format!("{}.json", conversation.id));
        assert!(!checkpoint.exists());
        state.flush_checkpoints().unwrap();
        assert!(
            checkpoint.exists(),
            "a clean exit must leave a current checkpoint"
        );
        // And it carries the watermark, so the next resume replays nothing.
        let raw = std::fs::read_to_string(&checkpoint).unwrap();
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(
            value.get("checkpoint_seq").is_some(),
            "a flushed checkpoint must be placeable in its log: {raw}"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_failed_append_keeps_its_events_for_the_next_save() {
        // The append is the save now, so a dropped batch is lost data --
        // the reducer drains its buffer at emission and never re-offers it.
        let root = std::env::temp_dir().join(format!(
            "mermaid-unappended-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let manager = crate::session::ConversationManager::new(&root).unwrap();
        let mut conversation = mermaid_domain::ConversationHistory::new(
            root.display().to_string(),
            "test/model".to_string(),
            chrono::Local::now(),
        );
        let mut state = PersistenceState::new(root.clone());

        // Block the log: a directory where its file goes.
        std::fs::create_dir_all(
            manager
                .conversations_dir()
                .join(format!("{}.jsonl", conversation.id)),
        )
        .expect("plant a blocker");
        let job = one_message_save(&mut conversation, "must survive");
        assert!(state.process(job).1.is_err(), "the append must fail");
        assert_eq!(
            state.unappended.get(&conversation.id).map(Vec::len),
            Some(1),
            "the batch must be held, not dropped"
        );

        // Unblock, save again: the held event goes first and both land.
        std::fs::remove_dir_all(
            manager
                .conversations_dir()
                .join(format!("{}.jsonl", conversation.id)),
        )
        .expect("unblock");
        state
            .process(one_message_save(&mut conversation, "and this one"))
            .1
            .unwrap();
        assert!(state.unappended.is_empty(), "the hold must clear");
        state.flush_checkpoints().unwrap();

        let resumed = manager.load_conversation(&conversation.id).unwrap();
        let texts: Vec<&str> = resumed
            .messages()
            .iter()
            .map(|m| m.content.as_str())
            .collect();
        assert!(
            texts.contains(&"must survive"),
            "the event held over a failed append must reach the log: {texts:?}"
        );
        assert!(texts.contains(&"and this one"), "{texts:?}");
        let _ = std::fs::remove_dir_all(root);
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
        let reply = mermaid_model::models::ChatMessage::assistant("new assistant reply");
        newer.add_messages(std::slice::from_ref(&reply), chrono::Local::now());
        // The event, not just the snapshot: the log is the truth now, and a
        // save below the checkpoint threshold writes nothing else. A fixture
        // that passed an empty batch would be asserting against a file this
        // save no longer touches.
        let (_, outcome) = state.process(PersistenceJob::Conversation {
            snapshot: Box::new(newer),
            events: vec![mermaid_domain::SessionEvent::Message { message: reply }],
        });
        outcome.unwrap();

        let loaded = crate::session::ConversationManager::new(&root)
            .unwrap()
            .load_conversation(&full.id)
            .unwrap();
        assert!(
            loaded
                .messages()
                .iter()
                .any(|message| message.content == "new assistant reply")
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn failed_event_append_blocks_later_stripped_conversation_save() {
        let root = std::env::temp_dir().join(format!(
            "mermaid-persistence-barrier-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let (full, compaction) = persistence_fixture(&root, "compact_blocked");
        let manager = crate::session::ConversationManager::new(&root).unwrap();
        manager.save_conversation(&full).unwrap();
        block_event_log(&root, &full.id);

        let mut state = PersistenceState::new(root.clone());
        assert!(
            state
                .process(PersistenceJob::Compaction(Box::new(compaction.clone())))
                .1
                .is_err()
        );
        assert!(
            state
                .process(PersistenceJob::Conversation {
                    snapshot: Box::new(compaction.conversation),
                    events: Vec::new(),
                })
                .1
                .is_err()
        );
        assert_eq!(state.blocked.get(&full.id).map(VecDeque::len), Some(1));

        let loaded = crate::session::ConversationManager::new(&root)
            .unwrap()
            .load_conversation(&full.id)
            .unwrap();
        assert_eq!(loaded.messages()[0].content, "raw history");
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
        let (full, first) = persistence_fixture(&root, "compact_first");
        let mut second = first.clone();
        second.record.id = "compact_second".to_string();
        block_event_log(&root, &full.id);

        let mut state = PersistenceState::new(root.clone());
        assert!(
            state
                .process(PersistenceJob::Compaction(Box::new(first)))
                .1
                .is_err()
        );
        // The older barrier still fails; the new save must queue behind it —
        // its boundary event is the only record of the stripped messages.
        assert!(
            state
                .process(PersistenceJob::Compaction(Box::new(second)))
                .1
                .is_err()
        );
        let queued = state.blocked.get(&full.id).expect("barrier queue");
        assert_eq!(queued.len(), 2);
        assert_eq!(queued[0].record.id, "compact_first");
        assert_eq!(queued[1].record.id, "compact_second");
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
        let (bad_full, bad) = persistence_fixture(&root, "compact_bad");
        let (mut good_full, mut good) = persistence_fixture(&root, "compact_good");
        block_event_log(&root, &bad_full.id);
        // Conversation ids are millisecond timestamps; two fixtures minted in
        // the same instant would collide into one barrier queue. Force the
        // second conversation onto a distinct (still format-valid) id.
        good_full.id = "20990101_000000_001".to_string();
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
        assert_eq!(loaded.messages()[0].content, "compacted checkpoint");
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
        bad.record.id = "compact_bad".to_string();
        // Both saves sit in ONE queue, so the blocker cannot be the shared
        // log path: give the tail an id that fails validation instead.
        bad.conversation.id = "../invalid".to_string();

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
