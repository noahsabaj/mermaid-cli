//! Central safety-policy gate shared by every tool that can mutate the
//! workspace, touch the network, drive the desktop, or spawn work.
//!
//! Before v0.7.x the `PolicyEngine` was only consulted by `execute_command`
//! and the filesystem mutators; `web_*`, `mcp`, `subagent`, and the
//! computer-use tools ran their bodies with no policy check at all, so
//! `SafetyMode::ReadOnly` silently failed to block them. This module is the
//! single choke point: every dangerous tool builds an [`ActionRequest`] and
//! calls [`gate`] before acting.
//!
//! `replayable` distinguishes the two enforcement shapes:
//!
//! - **Replayable tools** (`execute_command`, file mutators): an `Ask` (or an
//!   escalated Auto-mode `Classify`) decision creates a checkpoint + an
//!   approval row and BLOCKS, returning an "approval required" outcome. The
//!   action is later re-run out-of-band by [`crate::runtime::approve_and_replay`].
//! - **Non-replayable tools** (`web_*`, `mcp`, `subagent`, computer-use):
//!   there is no checkpoint/replay path, so an `Ask` can't be satisfied
//!   out-of-band — they proceed on `Ask` and are blocked only by `ReadOnly`
//!   (or a `Deny` override). The meaningful safety knob for these tools is
//!   `ReadOnly`.
//!
//! `Auto` mode resolves a `PolicyDecision::Classify` here by awaiting the
//! injected `AutoClassifier` (in `ctx.classifier`): aligned ⇒ proceed,
//! otherwise escalate — a replayable tool to a human approval, a non-replayable
//! tool to a block (it can't be replayed). A missing classifier or any vet
//! failure fails safe to escalate. This is why `gate` is `async`.

use std::path::PathBuf;

use crate::domain::ToolOutcome;
use crate::runtime::{
    ActionRequest, NewApproval, PolicyDecision, PolicyEngine, RiskClass, RuntimeStore,
    create_checkpoint_for_task, run_plugin_hooks,
};

use super::super::ctx::ExecContext;

/// Result of consulting the policy for a tool action.
pub enum Gate {
    /// The tool may run. `risk` is the classified risk (callers that take
    /// their own post-approval checkpoint, like `execute_command`, use it to
    /// decide whether to snapshot).
    Proceed { risk: RiskClass },
    /// The tool must NOT run; return this outcome verbatim to the model.
    Block(ToolOutcome),
}

/// Convenience for non-replayable tools (`web_*`, `mcp`, `subagent`,
/// computer-use): consult the policy and return `Some(outcome)` when the
/// action is blocked (e.g. `ReadOnly`/`Deny` override), or `None` to proceed.
/// These tools have no checkpoint/replay path, so `Ask` proceeds — only
/// `Deny` blocks them. Call this at the very top of `execute()`.
pub async fn gate_external(
    ctx: &ExecContext,
    tool: &'static str,
    category: crate::runtime::ToolCategory,
    summary: String,
    args: &serde_json::Value,
) -> Option<ToolOutcome> {
    let request = ActionRequest::new(tool, category, summary);
    let pending = serde_json::json!({ "tool": tool, "args": args });
    match gate(ctx, request, &[], pending, false).await {
        Gate::Block(outcome) => Some(outcome),
        Gate::Proceed { .. } => None,
    }
}

/// Consult the safety policy for `request`. See the module docs for the
/// `replayable` semantics.
pub async fn gate(
    ctx: &ExecContext,
    request: ActionRequest,
    checkpoint_paths: &[PathBuf],
    pending_action: serde_json::Value,
    replayable: bool,
) -> Gate {
    // Build from the LIVE session mode (Shift+Tab / `/safety` take effect
    // immediately), not the static config snapshot.
    let decision = PolicyEngine::new(ctx.safety_mode)
        .with_overrides(ctx.config.safety.overrides.clone())
        .decide(&request);

    match decision {
        PolicyDecision::Allow { risk, .. } => Gate::Proceed { risk },
        PolicyDecision::Ask { risk, checkpoint } => {
            if !replayable {
                // No checkpoint/replay path exists for this tool, so an Ask
                // can't be satisfied out-of-band. ReadOnly already Denied
                // (handled below); for Ask we proceed.
                tracing::debug!(
                    tool = %request.tool,
                    "policy Ask on non-replayable tool; proceeding (only ReadOnly/Deny blocks it)",
                );
                return Gate::Proceed { risk };
            }
            block_for_approval(
                ctx,
                &request,
                checkpoint,
                checkpoint_paths,
                pending_action,
                risk,
                None,
            )
        },
        PolicyDecision::Classify { risk, checkpoint } => {
            // Auto mode: an LLM vets the borderline action against the user's
            // intent. Aligned ⇒ proceed; otherwise escalate (fail-safe).
            let verdict = match &ctx.classifier {
                Some(classifier) => {
                    let vreq = crate::providers::VetRequest {
                        tool: request.tool.clone(),
                        summary: request.summary.clone(),
                        command: request.command.clone(),
                        path: request.path.clone(),
                        intent: ctx.intent.clone(),
                        workdir: ctx.workdir.display().to_string(),
                        turn: ctx.turn,
                        token: ctx.token.clone(),
                    };
                    classifier.vet(&vreq).await
                },
                None => crate::providers::VetVerdict::escalate("no Auto-mode classifier available"),
            };
            if verdict.allow {
                Gate::Proceed { risk }
            } else if replayable {
                // Escalate to a human approval the user can approve + replay.
                block_for_approval(
                    ctx,
                    &request,
                    checkpoint,
                    checkpoint_paths,
                    pending_action,
                    risk,
                    Some(verdict.reason),
                )
            } else {
                // Non-replayable: an approval can't be satisfied out-of-band,
                // so block with the classifier's reason for the model to see.
                Gate::Block(ToolOutcome::error(
                    format!(
                        "{} blocked by Auto-mode safety review: {}",
                        request.summary, verdict.reason
                    ),
                    0.0,
                ))
            }
        },
        PolicyDecision::Deny { reason, .. } => Gate::Block(ToolOutcome::error(
            format!("{} blocked by policy: {}", request.summary, reason),
            0.0,
        )),
    }
}

/// Take a checkpoint (when configured), record an approval row, and return a
/// blocking "approval required" outcome. Mirrors the pre-existing inline logic
/// from `exec.rs`/`filesystem.rs` so behavior is unchanged for those tools.
#[allow(clippy::too_many_arguments)]
fn block_for_approval(
    ctx: &ExecContext,
    request: &ActionRequest,
    checkpoint: bool,
    checkpoint_paths: &[PathBuf],
    pending_action: serde_json::Value,
    risk: RiskClass,
    // When the escalation came from the Auto-mode classifier, its reason —
    // recorded on the approval so the user sees *why* it was flagged.
    classifier_reason: Option<String>,
) -> Gate {
    let checkpoint_id = if checkpoint && ctx.config.safety.checkpoint_on_mutation {
        match create_checkpoint_for_task(
            &ctx.workdir,
            checkpoint_paths,
            Some(pending_action.clone()),
            ctx.task_id.clone(),
        ) {
            Ok(manifest) => Some(manifest.id),
            Err(error) => {
                return Gate::Block(ToolOutcome::error(
                    format!(
                        "{} checkpoint failed before approval: {}",
                        request.summary, error
                    ),
                    0.0,
                ));
            },
        }
    } else {
        None
    };

    let args_summary = request
        .command
        .clone()
        .or_else(|| request.path.clone())
        .unwrap_or_else(|| request.summary.clone());
    let pending_action_json = serde_json::to_string(&pending_action).ok();
    let tool = request.tool.clone();
    let risk_str = risk.as_str().to_string();

    let proposed_action = match &classifier_reason {
        Some(reason) => format!("{} [auto-review: {}]", request.summary, reason),
        None => request.summary.clone(),
    };

    let approval_id = RuntimeStore::open_default()
        .and_then(|store| {
            let approval = store.approvals().create(NewApproval {
                task_id: ctx.task_id.clone(),
                proposed_action: proposed_action.clone(),
                risk_classification: risk_str.clone(),
                policy_decision: "ask".to_string(),
                args_summary: Some(args_summary),
                checkpoint_id: checkpoint_id.clone(),
                pending_action_json,
            })?;
            if let Some(checkpoint_id) = checkpoint_id.as_deref() {
                let _ = store
                    .checkpoints()
                    .set_approval(checkpoint_id, &approval.id);
            }
            let _ = run_plugin_hooks(
                "approval_requested",
                &serde_json::json!({
                    "id": approval.id.clone(),
                    "task_id": approval.task_id.clone(),
                    "tool": tool,
                    "risk": risk_str,
                    "checkpoint_id": checkpoint_id.clone(),
                }),
            );
            Ok(approval)
        })
        .map(|approval| approval.id)
        .ok();

    Gate::Block(ToolOutcome::error(
        format!(
            "Approval required for {}{}",
            request.summary,
            approval_id
                .map(|id| format!(" (approval {})", id))
                .unwrap_or_default()
        ),
        0.0,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ToolCallId, TurnId};
    use crate::providers::ctx::ProgressEvent;
    use crate::runtime::{SafetyMode, ToolCategory};
    use std::path::PathBuf;
    use std::sync::Arc;

    fn ctx(mode: SafetyMode) -> ExecContext {
        let mut config = crate::app::Config::default();
        config.safety.mode = mode;
        let (tx, _rx) = tokio::sync::mpsc::channel::<ProgressEvent>(4);
        ExecContext::new(
            tokio_util::sync::CancellationToken::new(),
            tx,
            ToolCallId(1),
            TurnId(1),
            PathBuf::from("."),
            Arc::new(config),
            String::new(),
            None,
            mode,
            None,
            None,
        )
    }

    /// Stub classifier with a fixed verdict — drives the `Classify` path
    /// without a real model call.
    struct StubClassifier {
        allow: bool,
    }

    #[async_trait::async_trait]
    impl crate::providers::AutoClassifier for StubClassifier {
        async fn vet(&self, _req: &crate::providers::VetRequest) -> crate::providers::VetVerdict {
            if self.allow {
                crate::providers::VetVerdict::allow()
            } else {
                crate::providers::VetVerdict::escalate("stub: misaligned")
            }
        }
    }

    fn ctx_auto(classifier: Option<Arc<dyn crate::providers::AutoClassifier>>) -> ExecContext {
        let mut config = crate::app::Config::default();
        config.safety.mode = SafetyMode::Auto;
        let (tx, _rx) = tokio::sync::mpsc::channel::<ProgressEvent>(4);
        ExecContext::new(
            tokio_util::sync::CancellationToken::new(),
            tx,
            ToolCallId(1),
            TurnId(1),
            PathBuf::from("."),
            Arc::new(config),
            String::new(),
            None,
            SafetyMode::Auto,
            Some("fetch the changelog".to_string()),
            classifier,
        )
    }

    #[tokio::test]
    async fn readonly_blocks_external_tools() {
        // C1/H1/H2: the previously-bypassing tools must be denied in ReadOnly.
        let ctx = ctx(SafetyMode::ReadOnly);
        for (tool, cat) in [
            ("web_fetch", ToolCategory::Web),
            ("mcp_proxy", ToolCategory::Mcp),
            ("agent", ToolCategory::Subagent),
            ("click", ToolCategory::ComputerUse),
        ] {
            assert!(
                gate_external(&ctx, tool, cat, tool.to_string(), &serde_json::json!({}))
                    .await
                    .is_some(),
                "ReadOnly must block {tool}",
            );
        }
    }

    #[tokio::test]
    async fn full_access_allows_external_tools() {
        let ctx = ctx(SafetyMode::FullAccess);
        assert!(
            gate_external(
                &ctx,
                "web_fetch",
                ToolCategory::Web,
                "web_fetch".to_string(),
                &serde_json::json!({}),
            )
            .await
            .is_none()
        );
    }

    #[tokio::test]
    async fn auto_classifier_allow_proceeds() {
        // Auto + classifier says ALLOW ⇒ a borderline external tool proceeds.
        let ctx = ctx_auto(Some(Arc::new(StubClassifier { allow: true })));
        assert!(
            gate_external(
                &ctx,
                "web_fetch",
                ToolCategory::Web,
                "web_fetch".to_string(),
                &serde_json::json!({}),
            )
            .await
            .is_none(),
            "ALLOW verdict should let the action proceed",
        );
    }

    #[tokio::test]
    async fn auto_classifier_escalate_blocks() {
        // Auto + classifier says ESCALATE ⇒ a non-replayable tool is blocked.
        let ctx = ctx_auto(Some(Arc::new(StubClassifier { allow: false })));
        assert!(
            gate_external(
                &ctx,
                "web_fetch",
                ToolCategory::Web,
                "web_fetch".to_string(),
                &serde_json::json!({}),
            )
            .await
            .is_some(),
            "ESCALATE verdict should block a non-replayable tool",
        );
    }

    #[tokio::test]
    async fn auto_without_classifier_fails_safe() {
        // Auto but no classifier bound ⇒ fail safe (escalate ⇒ block), never
        // silently allow.
        let ctx = ctx_auto(None);
        assert!(
            gate_external(
                &ctx,
                "web_fetch",
                ToolCategory::Web,
                "web_fetch".to_string(),
                &serde_json::json!({}),
            )
            .await
            .is_some(),
            "missing classifier must fail safe (block), not allow",
        );
    }
}
