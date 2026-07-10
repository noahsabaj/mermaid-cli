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

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::domain::{Msg, ToolCallId, TurnId};
use crate::models::tool_call::ToolCall as ModelToolCall;
use crate::models::{ChatMessage, FinishReason, ProviderContinuation, ReasoningChunk, TokenUsage};
use crate::runtime::SafetyMode;

use super::approval::ApprovalBroker;
use super::auto_classifier::AutoClassifier;
use super::questions::QuestionBroker;

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
    /// detach a running child (execute_command, agent) select on it; the live
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
    /// Parent session's `app::Config`. Needed by `SubagentTool` so the
    /// child reducer uses the same Ollama host, reasoning prefs, MCP
    /// servers, etc. Other tools don't consult it — keeping it as a
    /// typed field (rather than a global) means the dependency is
    /// explicit in the signature.
    pub config: Arc<crate::app::Config>,
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
    /// Effective live safety mode for this call (from the session, not the
    /// static config). The policy gate builds its `PolicyEngine` from this.
    pub safety_mode: SafetyMode,
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
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        token: CancellationToken,
        progress: mpsc::Sender<ProgressEvent>,
        call_id: ToolCallId,
        turn: TurnId,
        workdir: PathBuf,
        config: Arc<crate::app::Config>,
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
        }
    }

    /// Checkpoint provenance for this call — every checkpoint-creating tool
    /// passes this so file snapshots anchor to the conversation position
    /// that produced them (rewind/fork surfaces them by anchor).
    pub fn checkpoint_origin(&self) -> crate::runtime::CheckpointOrigin {
        crate::runtime::CheckpointOrigin {
            task_id: self.task_id.clone(),
            session_id: self.session_id.clone(),
            message_index: self.message_index,
        }
    }
}

/// Tool-side progress event. The reducer already knows `ToolStarted`
/// and `ToolFinished`; this carries everything in between (streaming
/// subprocess output, long-running download status, multimodal
/// artifacts like inline screenshots, and nested activity from
/// subagents).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum ProgressEvent {
    /// Partial stdout/stderr chunk.
    Output(String),
    /// Arbitrary status string for display.
    Status(String),
    /// Byte-count progress for long downloads/transfers. `total` is
    /// None when the producer doesn't know the final size.
    Bytes { done: u64, total: Option<u64> },
    /// Binary artifact produced mid-execution (screenshot preview,
    /// generated file, etc.). MIME string determines routing in the
    /// reducer — `image/*` attaches inline to the active assistant
    /// message; anything else lands on the status line as a label.
    Artifact {
        mime: String,
        #[serde(with = "crate::utils::serde_base64")]
        data: Vec<u8>,
        caption: Option<String>,
    },
    /// A child subagent just started or finished a tool call. Carries
    /// the CHILD's call identity + tool name + phase so the parent UI
    /// can surface it without needing to recurse into the child's
    /// event vocabulary.
    SubagentToolCall {
        child_call_id: ToolCallId,
        tool_name: String,
        phase: SubagentPhase,
    },
    /// Coarse phase label for a child subagent ("starting…",
    /// "thinking", "replying"). Emitted only on phase CHANGE — never
    /// per stream chunk — so the parent status stays calm.
    SubagentActivity(String),
    /// Cumulative output-token estimate for a child subagent's current
    /// drive. Throttled at the source (≥500ms apart); powers the live
    /// per-agent token counters without per-chunk churn.
    SubagentTokens(usize),
}

/// Phase a subagent tool-call is in, from the parent's perspective.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SubagentPhase {
    Started,
    Finished,
    Errored,
}

/// Narrow shim from the reducer's `ChatRequest` to the adapter-facing
/// messages. Providers often want to mutate the last assistant
/// message (e.g. Anthropic cache_control injection); this helper
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
    let mut config = crate::app::Config::default();
    config.safety.mode = crate::runtime::SafetyMode::FullAccess;
    test_exec_context_with_config(turn, call_id, workdir, config)
}

/// [`test_exec_context`] with an explicit `Config` (e.g. `exec.pty = false`
/// to pin the pipe spawn path).
pub fn test_exec_context_with_config(
    turn: TurnId,
    call_id: ToolCallId,
    workdir: PathBuf,
    config: crate::app::Config,
) -> (ExecContext, mpsc::Receiver<ProgressEvent>) {
    let token = CancellationToken::new();
    let (tx, rx) = mpsc::channel(64);
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
            crate::runtime::SafetyMode::FullAccess,
            None,
            None,
            None,
            None,
            None,
        ),
        rx,
    )
}

/// Convenience: build a Send+Sync sharable sink for tests.
pub fn arc_sink(tx: mpsc::Sender<StreamEvent>) -> Arc<mpsc::Sender<StreamEvent>> {
    Arc::new(tx)
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
}
