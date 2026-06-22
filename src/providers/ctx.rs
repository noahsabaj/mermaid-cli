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

use crate::domain::{ToolCallId, TurnId};
use crate::models::tool_call::ToolCall as ModelToolCall;
use crate::models::{ChatMessage, ReasoningChunk, TokenUsage};
use crate::runtime::SafetyMode;

use super::approval::ApprovalBroker;
use super::auto_classifier::AutoClassifier;

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
    /// Optional — some providers emit a signature mid-stream
    /// (Anthropic). Adapters that only have it at the end can attach
    /// it to `Done` instead.
    ThinkingSignature(String),
    /// Stream complete. Carries final token usage (None if unknown)
    /// and any terminal thinking signature.
    Done {
        usage: Option<TokenUsage>,
        thinking_signature: Option<String>,
    },
}

/// Final response returned by `ModelProvider::chat()` after the
/// stream drains. Carries what the reducer can't derive from the
/// stream events themselves — token usage and the opaque thinking
/// signature needed for Anthropic extended-thinking continuation.
#[derive(Debug, Clone)]
pub struct FinalResponse {
    pub usage: Option<TokenUsage>,
    pub thinking_signature: Option<String>,
    pub tool_calls: Vec<ModelToolCall>,
}

/// What a `ToolExecutor::execute()` receives.
pub struct ExecContext {
    pub token: CancellationToken,
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
        safety_mode: SafetyMode,
        intent: Option<String>,
        classifier: Option<Arc<dyn AutoClassifier>>,
        approval: Option<ApprovalBroker>,
    ) -> Self {
        Self {
            token,
            progress,
            call_id,
            turn,
            workdir,
            config,
            model_id,
            task_id,
            safety_mode,
            intent,
            classifier,
            approval,
        }
    }
}

/// Tool-side progress event. The reducer already knows `ToolStarted`
/// and `ToolFinished`; this carries everything in between (streaming
/// subprocess output, long-running download status, multimodal
/// artifacts like inline screenshots, and nested activity from
/// subagents).
#[derive(Debug, Clone)]
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
    /// A chunk of assistant text produced by a child subagent. Mostly
    /// UI flavor — lets the parent status line show what the sub is
    /// "saying" in real time.
    SubagentText(String),
}

/// Phase a subagent tool-call is in, from the parent's perspective.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    let token = CancellationToken::new();
    let (tx, rx) = mpsc::channel(64);
    let mut config = crate::app::Config::default();
    config.safety.mode = crate::runtime::SafetyMode::FullAccess;
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
            crate::runtime::SafetyMode::FullAccess,
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
