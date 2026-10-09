//! The `/goal` check: one model call, no tools, its raw reply sent back as
//! `Msg::GoalEvaluated`. The reducer parses it, so a recording replays the
//! same verdict.
use std::sync::Arc;
use std::time::Duration;

use super::*;

/// How long one check may take before it counts as failed. A failed check
/// pauses the goal; it never continues or ends it.
const CHECK_TIMEOUT: Duration = Duration::from_secs(120);

pub(super) async fn evaluate_goal(
    tx: MsgSender,
    providers: Option<Arc<ProviderFactory>>,
    turn: TurnId,
    request: mermaid_domain::ChatRequest,
    token: tokio_util::sync::CancellationToken,
) {
    let reply = check(providers, turn, request, token).await;
    let _ = tx.send(Msg::GoalEvaluated { turn, reply }).await;
}

async fn check(
    providers: Option<Arc<ProviderFactory>>,
    turn: TurnId,
    request: mermaid_domain::ChatRequest,
    token: tokio_util::sync::CancellationToken,
) -> Result<mermaid_domain::goal::GoalReply, String> {
    let factory = providers.ok_or_else(|| "no model provider in this session".to_string())?;
    let provider = factory
        .resolve(&request.model_id)
        .await
        .map_err(|e| e.to_string())?;
    let collected = tokio::time::timeout(
        CHECK_TIMEOUT,
        crate::providers::model::collect_text(provider, turn, request, token),
    )
    .await
    .map_err(|_| format!("no reply in {}s", CHECK_TIMEOUT.as_secs()))?
    .map_err(|e| mermaid_model::utils::redact_secrets(&e.to_string()))?;
    Ok(mermaid_domain::goal::GoalReply {
        text: collected.text,
        reasoning: collected.reasoning,
        usage: collected.usage,
    })
}
