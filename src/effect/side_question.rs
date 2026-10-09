//! `/btw`: answer one side question beside the main run.
//!
//! A detached task, not turn-scoped: the main turn neither waits for it nor
//! cancels it. The answer streams back as `Msg::SideQuestionText` and ends
//! with one `Msg::SideQuestionFinished`. Nothing here touches the
//! conversation or its files.
use std::sync::Arc;

use mermaid_domain::side_question::SideOutcome;

use super::*;

pub(super) async fn answer_side_question(
    tx: MsgSender,
    providers: Option<Arc<ProviderFactory>>,
    id: u64,
    mut request: mermaid_domain::ChatRequest,
) {
    let outcome = match providers {
        None => SideOutcome::Failed("No model provider is bound in this session.".to_string()),
        Some(factory) => stream_answer(&tx, &factory, id, &mut request).await,
    };
    let _ = tx.send(Msg::SideQuestionFinished { id, outcome }).await;
}

async fn stream_answer(
    tx: &MsgSender,
    factory: &ProviderFactory,
    id: u64,
    request: &mut mermaid_domain::ChatRequest,
) -> SideOutcome {
    let provider = match factory.resolve(&request.model_id).await {
        Ok(p) => p,
        Err(e) => return SideOutcome::Failed(e.to_string()),
    };
    // Shape the request the way the main call does, so the provider sees the
    // same prefix and a warm prompt cache still applies.
    if !provider.capabilities().supports_tools {
        request.tools.clear();
    }
    let sizing = provider.resolve_context_window(request).await;
    request.resolved_context_window = sizing.effective.or(sizing.model_max);
    request.resolved_max_output = sizing.max_output;
    request.native_tools = native_tools_for(factory.config());

    let (stream_tx, mut stream_rx) = mpsc::channel::<StreamEvent>(128);
    // The token is never cancelled: a side question ends on its own or with
    // the process. `TurnId(0)` only labels the stream; nothing gates on it.
    let ctx = StreamContext::new(
        tokio_util::sync::CancellationToken::new(),
        stream_tx,
        TurnId(0),
    );
    let relay_tx = tx.clone();
    let relay = tokio::spawn(async move {
        let mut tried_tools = false;
        while let Some(event) = stream_rx.recv().await {
            match event {
                StreamEvent::Text(chunk) => {
                    let _ = relay_tx.send(Msg::SideQuestionText { id, chunk }).await;
                },
                // Side questions run no tools; the answer notes the request.
                StreamEvent::ToolCall(_) => tried_tools = true,
                StreamEvent::Reasoning(_) | StreamEvent::Status(_) | StreamEvent::Done { .. } => {},
            }
        }
        tried_tools
    });
    let result = provider.chat(request.clone(), ctx).await;
    let tried_tools = relay.await.unwrap_or(false);
    match result {
        Ok(_) => SideOutcome::Done { tried_tools },
        Err(e) => {
            let error = e.to_user_facing();
            SideOutcome::Failed(format!("{}: {}", error.summary, error.message))
        },
    }
}

/// `f` in the `/btw` pane: start a background agent that carries on from the
/// side answer with full tools. The agent detaches at once (its background
/// token is already fired), so it reports the way a Ctrl+B agent does:
/// `Msg::BackgroundAgentStarted` now, `Msg::BackgroundAgentFinished` later.
pub(super) async fn fork_side_question(
    tx: MsgSender,
    tool: crate::providers::tool::subagent::SubagentTool,
    fork: SideFork,
) {
    let SideFork {
        prompt,
        description,
        history,
        dispatch,
        services,
    } = fork;
    // Nobody reads the turn progress of a detached agent; the receiver can go.
    let (progress_tx, _) = mpsc::channel(1);
    let background = tokio_util::sync::CancellationToken::new();
    background.cancel();
    let signals = crate::providers::ctx::TurnSignals {
        token: tokio_util::sync::CancellationToken::new(),
        background,
        web_bytes: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    };
    let ctx = ExecContext::assemble(
        TurnId(0),
        mermaid_domain::ToolCallId(0),
        progress_tx,
        signals,
        dispatch,
        services,
    );
    let args = serde_json::json!({ "prompt": prompt, "description": description });
    let outcome = tool.run(args, ctx, Some(history)).await;
    // A detach notifies on its own. Anything else stopped before the agent
    // started (no free slot, a bad agent type): say why.
    if !outcome.is_success() {
        let _ = tx
            .send(Msg::TransientStatus {
                text: format!("The fork did not start: {}", outcome.summary),
            })
            .await;
    }
}

/// What `fork_side_question` needs besides the tool.
pub(super) struct SideFork {
    pub prompt: String,
    pub description: String,
    pub history: Vec<mermaid_model::models::ChatMessage>,
    pub dispatch: mermaid_domain::ToolDispatch,
    pub services: crate::providers::ctx::ToolServices,
}
