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
            let error = classify_error_for_ui(&e);
            SideOutcome::Failed(format!("{}: {}", error.summary, error.message))
        },
    }
}
