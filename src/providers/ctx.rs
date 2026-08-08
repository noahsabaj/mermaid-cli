//! Per-call context passed to providers and tool executors.
//!
//! The two structs below are the single point where per-turn
//! cancellation + progress reporting + session identity meet a
//! specific provider call. Everything a model or tool adapter needs
//! to participate in structured concurrency is here.
//!
//! - `StreamContext` is handed to a `ModelProvider::chat()`. It
//!   carries the cancellation token for the turn and a bounded mpsc
//!   sink for streaming events. The adapter `select!`s on
//!   `token.cancelled()` inside its read loop and awaits
//!   `sink.send(event)` — if the main loop is drowning, the `await`
//!   applies natural backpressure and the provider's TCP buffer fills
//!   instead of the channel growing unbounded.
//!
//! - `ExecContext` is handed to a `ToolExecutor::execute()`. Same
//!   token (so Ctrl+C cancels tools too) plus a progress sink and
//!   identifiers so the reducer can match results to the call that
//!   produced them.

use mermaid_domain::ProgressEvent;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use mermaid_domain::{Msg, ToolCallId, TurnId};
use mermaid_model::models::tool_call::ToolCall as ModelToolCall;
use mermaid_model::models::{
    ChatMessage, FinishReason, ProviderContinuation, ReasoningChunk, TokenUsage,
};
use mermaid_runtime::SafetyMode;

use super::approval::ApprovalBroker;
use super::auto_classifier::AutoClassifier;
use super::questions::QuestionBroker;

/// Shared, byte-exact budget for decoded HTTP response data in one turn.
/// Clones point at the same atomic counter, so parallel tool calls and batched
/// queries cannot each claim the full allowance independently.
#[derive(Clone, Debug)]
pub struct WebByteBudget {
    used: Arc<AtomicUsize>,
}

impl WebByteBudget {
    pub(crate) fn shared(used: Arc<AtomicUsize>) -> Self {
        Self { used }
    }

    #[cfg(test)]
    pub(crate) fn isolated() -> Self {
        Self::shared(Arc::new(AtomicUsize::new(0)))
    }

    /// Charge decoded bytes without allowing the shared total to cross the
    /// fixed per-turn limit. An overflowing charge atomically saturates the
    /// counter so every later response observes an exhausted budget before it
    /// polls another body.
    // Nightly renamed `fetch_update` to `try_update` and deprecated the old
    // name. `try_update` is not stable, so the call cannot be migrated yet and
    // the deprecation cannot be avoided — and since the `[lints.rust]
    // warnings = "deny"` table landed in every manifest, a warning the nightly
    // toolchain emits is a hard error in the test build now, not only in
    // clippy. That is what turned this into a red nightly leg.
    //
    // `#[allow]` and not `#[expect]`: on stable there is no deprecation to
    // fulfil, so an expectation would itself become the warning on the
    // toolchain that matters most. Delete both of these once `try_update`
    // reaches the MSRV.
    #[allow(deprecated, reason = "try_update is not stable yet; see above")]
    pub fn charge(&self, bytes: usize) -> Result<usize, usize> {
        let limit = mermaid_model::constants::MAX_WEB_TURN_BYTES;
        let prior = self
            .used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                Some(used.saturating_add(bytes).min(limit))
            })
            .expect("web byte budget update always supplies a value");
        let next = prior.saturating_add(bytes);
        if prior >= limit || next > limit {
            Err(limit)
        } else {
            Ok(next)
        }
    }

    pub fn remaining(&self) -> usize {
        mermaid_model::constants::MAX_WEB_TURN_BYTES
            .saturating_sub(self.used.load(Ordering::Acquire))
    }
}

/// What a `ModelProvider::chat()` receives.
#[derive(Debug)]
pub struct StreamContext {
    pub token: CancellationToken,
    pub sink: mpsc::Sender<StreamEvent>,
    pub turn: TurnId,
}

impl StreamContext {
    pub fn new(token: CancellationToken, sink: mpsc::Sender<StreamEvent>, turn: TurnId) -> Self {
        Self { token, sink, turn }
    }
}

/// One event emitted during a streaming model call. Adapters MUST
/// emit exactly one `Done` at the end of a successful stream. `Text`
/// and `Reasoning` may interleave. `ToolCall` events typically arrive
/// near the end but the contract is "before `Done`".
#[derive(Debug, Clone)]
pub enum StreamEvent {
    Text(String),
    Reasoning(ReasoningChunk),
    ToolCall(ModelToolCall),
    /// Out-of-band, user-visible plumbing notice (e.g. "Starting the local
    /// Ollama server…"). Not response content — the effect layer routes it
    /// to a transient/system line, never into the assistant message.
    Status(String),
    /// Stream complete. Carries final token usage (None if unknown),
    /// any provider continuation state, and why generation stopped
    /// (so the reducer can flag truncation / a content block).
    Done {
        usage: Option<TokenUsage>,
        provider_continuation: Option<ProviderContinuation>,
        stop_reason: Option<FinishReason>,
    },
}

/// Final response returned by `ModelProvider::chat()` after the
/// stream drains. Carries what the reducer can't derive from the
/// stream events themselves: token usage and opaque provider continuation.
#[derive(Debug, Clone)]
pub struct FinalResponse {
    pub usage: Option<TokenUsage>,
    pub provider_continuation: Option<ProviderContinuation>,
    pub tool_calls: Vec<ModelToolCall>,
    pub stop_reason: Option<FinishReason>,
}

/// What a `ToolExecutor::execute()` receives.
pub struct ExecContext {
    pub token: CancellationToken,
    /// Ctrl+B "background this" signal, parallel to `token`. Tools that can
    /// detach a running child (`execute_command`, agent) select on it; the live
    /// path sets it from the turn scope, tests leave it never-fired.
    pub background: CancellationToken,
    /// Turn-independent channel back to the main reducer loop. Detached work
    /// (a backgrounded subagent) reports through this after the owning turn
    /// is gone — the per-turn `progress` channel dies with the turn. `None`
    /// in tests and contexts that never detach.
    pub notify: Option<mpsc::Sender<Msg>>,
    pub progress: mpsc::Sender<ProgressEvent>,
    pub call_id: ToolCallId,
    pub turn: TurnId,
    pub workdir: PathBuf,
    /// Parent session's `domain::Config`. Needed by `SubagentTool` so the
    /// child reducer uses the same Ollama host, reasoning prefs, MCP
    /// servers, etc. Other tools don't consult it — keeping it as a
    /// typed field (rather than a global) means the dependency is
    /// explicit in the signature.
    pub config: Arc<mermaid_domain::Config>,
    /// Parent session's active model id (e.g. `"anthropic/claude-opus-4-7"`).
    /// Subagents inherit this so they hit the same provider.
    pub model_id: String,
    /// Durable daemon task that owns this tool call, when execution was
    /// launched through the runtime task queue.
    pub task_id: Option<String>,
    /// Conversation id of the interactive session dispatching this call —
    /// stamped by the reducer onto `Cmd::ExecuteTool` so checkpoints can be
    /// anchored to a conversation position. `None` on headless/daemon paths.
    pub session_id: Option<String>,
    /// Conversation length (`messages().len()`) at dispatch; pairs with
    /// `session_id` for checkpoint anchoring (see `CheckpointOrigin`).
    pub message_index: Option<i64>,
    /// Per-session scratch directory, when the session has one materialized
    /// (`Msg::ScratchpadReady`). Stamped by the reducer onto
    /// `Cmd::ExecuteTool`; like `background`/`notify` it is field-set after
    /// construction on the live path — `None` in tests and before the
    /// directory is confirmed on disk.
    pub scratchpad: Option<PathBuf>,
    /// Effective live safety mode for this call (from the session, not the
    /// static config; floored to `ReadOnly` while a plan is being drafted).
    /// The policy gate builds its `PolicyEngine` from this.
    pub safety_mode: SafetyMode,
    /// `Some(path)` while the session is in plan mode: the one path the
    /// policy gate exempts from the read-only floor, and the flag the plan
    /// carve-outs (memory writes, known-safe builds) and the task tools key
    /// on. Defaults to `None` in `new` — the live dispatch path sets it,
    /// like `background`/`notify`.
    pub plan_file: Option<std::path::PathBuf>,
    /// LIVE per-category plan permission levels, threaded from the reducer
    /// (the frozen startup `config` would go stale under `/plan config`
    /// edits). Only consulted while `plan_file` is `Some`; defaults in `new`.
    pub plan_permissions: mermaid_domain::PlanPermissions,
    /// Context-window fill at dispatch, when known (`exit_plan_mode` shows
    /// it on the clear-context approval option). Defaults to `None` in `new`.
    pub context_percent: Option<u8>,
    /// The user's stated intent for the turn (latest user message), passed to
    /// the Auto-mode classifier so it can judge whether an action is aligned.
    pub intent: Option<String>,
    /// LLM classifier for `SafetyMode::Auto`. `Some` only when the effective
    /// mode is `Auto` and a provider is bound; the gate awaits it to resolve a
    /// `PolicyDecision::Classify`. `None` ⇒ the gate fails safe (escalate).
    pub classifier: Option<Arc<dyn AutoClassifier>>,
    /// Inline-approval back-channel (interactive runs only). `Some` lets the
    /// gate prompt the user and park until they answer; `None` (headless) falls
    /// back to the out-of-band DB-approval flow.
    pub approval: Option<ApprovalBroker>,
    /// Inline-question back-channel for `ask_user_question` (interactive runs
    /// only). `Some` lets the tool park until the user answers; `None`
    /// (headless) makes the tool proceed with best judgment instead of blocking.
    pub questions: Option<QuestionBroker>,
    /// The checklist broker for the task tools (single writer for all task
    /// state). Present on every live path — interactive, headless, and
    /// subagent runners each own one; `None` only in bare test contexts,
    /// where the tools degrade to a graceful no-op.
    pub tasks: Option<crate::providers::tasks::TaskBroker>,
    /// Decoded web bytes accepted by every sibling tool call in this turn.
    /// The effect runner replaces the constructor default with the owning
    /// `TurnScope` counter so parallel calls share one aggregate budget.
    pub web_bytes: Arc<AtomicUsize>,
}

impl std::fmt::Debug for ExecContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `classifier` is a trait object (no `Debug`); render its presence.
        f.debug_struct("ExecContext")
            .field("call_id", &self.call_id)
            .field("turn", &self.turn)
            .field("workdir", &self.workdir)
            .field("model_id", &self.model_id)
            .field("task_id", &self.task_id)
            .field("session_id", &self.session_id)
            .field("message_index", &self.message_index)
            .field("scratchpad", &self.scratchpad)
            .field("safety_mode", &self.safety_mode)
            .field("intent", &self.intent)
            .field(
                "classifier",
                &self.classifier.as_ref().map(|_| "<dyn AutoClassifier>"),
            )
            .field(
                "approval",
                &self.approval.as_ref().map(|_| "<ApprovalBroker>"),
            )
            .field(
                "questions",
                &self.questions.as_ref().map(|_| "<QuestionBroker>"),
            )
            .field("tasks", &self.tasks.as_ref().map(|_| "<TaskBroker>"))
            .finish_non_exhaustive()
    }
}

impl ExecContext {
    #[expect(clippy::too_many_arguments)]
    pub fn new(
        token: CancellationToken,
        progress: mpsc::Sender<ProgressEvent>,
        call_id: ToolCallId,
        turn: TurnId,
        workdir: PathBuf,
        config: Arc<mermaid_domain::Config>,
        model_id: String,
        task_id: Option<String>,
        session_id: Option<String>,
        message_index: Option<i64>,
        safety_mode: SafetyMode,
        intent: Option<String>,
        classifier: Option<Arc<dyn AutoClassifier>>,
        approval: Option<ApprovalBroker>,
        questions: Option<QuestionBroker>,
        tasks: Option<crate::providers::tasks::TaskBroker>,
    ) -> Self {
        Self {
            token,
            // Defaults to a fresh, never-fired token ("no background
            // requested"); the live execute path overwrites it with the turn
            // scope's background token (and sets `notify`).
            background: CancellationToken::new(),
            notify: None,
            plan_file: None,
            plan_permissions: mermaid_domain::PlanPermissions::default(),
            context_percent: None,
            // Field-set by the live execute path alongside `background`/
            // `notify`; tests and bare contexts leave it unset.
            scratchpad: None,
            progress,
            call_id,
            turn,
            workdir,
            config,
            model_id,
            task_id,
            session_id,
            message_index,
            safety_mode,
            intent,
            classifier,
            approval,
            questions,
            tasks,
            web_bytes: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Charge decoded web bytes to this turn without ever crossing the fixed
    /// aggregate limit. Returns the new total on success.
    pub fn charge_web_bytes(&self, bytes: usize) -> Result<usize, usize> {
        self.web_budget().charge(bytes)
    }

    /// A cloneable handle for transport code to charge each decoded chunk at
    /// the point it is accepted, including failed responses and retries.
    pub fn web_budget(&self) -> WebByteBudget {
        WebByteBudget::shared(self.web_bytes.clone())
    }

    /// Checkpoint provenance for this call — every checkpoint-creating tool
    /// passes this so file snapshots anchor to the conversation position
    /// that produced them (rewind/fork surfaces them by anchor).
    pub fn checkpoint_origin(&self) -> mermaid_runtime::CheckpointOrigin {
        mermaid_runtime::CheckpointOrigin {
            task_id: self.task_id.clone(),
            session_id: self.session_id.clone(),
            message_index: self.message_index,
        }
    }
}

/// Narrow shim from the reducer's `ChatRequest` to the adapter-facing
/// messages. Providers often want to mutate the last assistant
/// message (e.g. Anthropic `cache_control` injection); this helper
/// clones the slice as owned so the provider can do that without
/// fighting the borrow checker.
pub fn clone_messages(msgs: &[ChatMessage]) -> Vec<ChatMessage> {
    msgs.to_vec()
}

/// Builder that lets tests construct a pair of `StreamContext` +
/// receiver without needing a runtime. Used by provider unit tests
/// and by integration harnesses in C9.
pub fn test_stream_context(turn: TurnId) -> (StreamContext, mpsc::Receiver<StreamEvent>) {
    let token = CancellationToken::new();
    let (tx, rx) = mpsc::channel(64);
    (StreamContext::new(token, tx, turn), rx)
}

/// Builder counterpart for `ExecContext`. Uses a `Config` pinned to
/// `SafetyMode::FullAccess` (the production default is now `Ask`) so tool
/// unit tests exercise the tool's own behavior rather than the approval
/// gate. Tests that specifically exercise policy gating should construct
/// `ExecContext::new` directly with their chosen safety mode.
pub fn test_exec_context(
    turn: TurnId,
    call_id: ToolCallId,
    workdir: PathBuf,
) -> (ExecContext, mpsc::Receiver<ProgressEvent>) {
    let mut config = mermaid_domain::Config::default();
    config.safety.mode = mermaid_runtime::SafetyMode::FullAccess;
    test_exec_context_with_config(turn, call_id, workdir, config)
}

/// [`test_exec_context`] with an explicit `Config` (e.g. `exec.pty = false`
/// to pin the pipe spawn path, or a `safety.mode` other than `FullAccess`).
/// The context's safety mode follows `config.safety.mode`, so gate tests can
/// pick a mode without hand-rolling `ExecContext::new`.
pub fn test_exec_context_with_config(
    turn: TurnId,
    call_id: ToolCallId,
    workdir: PathBuf,
    config: mermaid_domain::Config,
) -> (ExecContext, mpsc::Receiver<ProgressEvent>) {
    let token = CancellationToken::new();
    let (tx, rx) = mpsc::channel(64);
    let safety_mode = config.safety.mode;
    let config = Arc::new(config);
    (
        ExecContext::new(
            token,
            tx,
            call_id,
            turn,
            workdir,
            config,
            String::new(),
            None,
            None,
            None,
            safety_mode,
            None,
            None,
            None,
            None,
            None,
        ),
        rx,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[tokio::test]
    async fn stream_context_carries_token_and_turn() {
        let (ctx, _rx) = test_stream_context(TurnId(5));
        assert_eq!(ctx.turn, TurnId(5));
        assert!(!ctx.token.is_cancelled());
    }

    #[tokio::test]
    async fn exec_context_propagates_cancel_signal() {
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(2), PathBuf::from("/tmp"));
        let token = ctx.token.clone();
        tokio::spawn(async move {
            token.cancel();
        });
        // Wait until cancelled.
        ctx.token.cancelled().await;
        assert!(ctx.token.is_cancelled());
    }

    #[tokio::test]
    async fn progress_event_round_trips_through_channel() {
        let (ctx, mut rx) = test_exec_context(TurnId(1), ToolCallId(2), PathBuf::from("/tmp"));
        ctx.progress
            .send(ProgressEvent::Status("halfway".to_string()))
            .await
            .expect("send");
        match rx.recv().await.expect("recv") {
            ProgressEvent::Status(s) => assert_eq!(s, "halfway"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn web_budget_is_atomic_and_never_crosses_the_turn_limit() {
        let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(2), PathBuf::from("/tmp"));
        assert_eq!(ctx.charge_web_bytes(1024), Ok(1024));
        let remaining = mermaid_model::constants::MAX_WEB_TURN_BYTES - 1024;
        assert_eq!(
            ctx.charge_web_bytes(remaining),
            Ok(mermaid_model::constants::MAX_WEB_TURN_BYTES)
        );
        assert_eq!(
            ctx.charge_web_bytes(1),
            Err(mermaid_model::constants::MAX_WEB_TURN_BYTES)
        );
    }

    #[test]
    fn web_budget_overflow_saturates_and_stays_exhausted() {
        let budget = WebByteBudget::isolated();
        let limit = mermaid_model::constants::MAX_WEB_TURN_BYTES;
        assert_eq!(budget.charge(limit - 1), Ok(limit - 1));
        assert_eq!(budget.charge(2), Err(limit));
        assert_eq!(budget.remaining(), 0);
        assert_eq!(budget.charge(0), Err(limit));
        assert_eq!(budget.charge(usize::MAX), Err(limit));
        assert_eq!(budget.remaining(), 0);
    }
}
