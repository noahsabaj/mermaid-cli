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
//! - **Replayable tools** (`execute_command`, file mutators): an `Ask`
//!   decision creates a checkpoint + an approval row and BLOCKS, returning an
//!   "approval required" outcome. The action is later re-run out-of-band by
//!   [`crate::runtime::approve_and_replay`].
//! - **Non-replayable tools** (`web_*`, `mcp`, `subagent`, computer-use):
//!   there is no checkpoint/replay path for these, so an `Ask` cannot be
//!   satisfied out-of-band. They are gated on **`Deny` only** — `ReadOnly`
//!   (and any `Deny` override) blocks them, which is exactly the hole the
//!   review flagged; `Ask`/`AutoReview`/`FullAccess` proceed. The meaningful
//!   safety knob for these tools is `ReadOnly`.

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
pub fn gate_external(
    ctx: &ExecContext,
    tool: &'static str,
    category: crate::runtime::ToolCategory,
    summary: String,
    args: &serde_json::Value,
) -> Option<ToolOutcome> {
    let request = ActionRequest::new(tool, category, summary);
    let pending = serde_json::json!({ "tool": tool, "args": args });
    match gate(ctx, request, &[], pending, false) {
        Gate::Block(outcome) => Some(outcome),
        Gate::Proceed { .. } => None,
    }
}

/// Consult the safety policy for `request`. See the module docs for the
/// `replayable` semantics.
pub fn gate(
    ctx: &ExecContext,
    request: ActionRequest,
    checkpoint_paths: &[PathBuf],
    pending_action: serde_json::Value,
    replayable: bool,
) -> Gate {
    let decision = PolicyEngine::new(ctx.config.safety.mode)
        .with_overrides(ctx.config.safety.overrides.clone())
        .decide(&request);

    match decision {
        PolicyDecision::Allow { risk, .. } => Gate::Proceed { risk },
        PolicyDecision::Ask { risk, checkpoint } => {
            if !replayable {
                // No checkpoint/replay path exists for this tool, so an Ask
                // can't be satisfied out-of-band. ReadOnly already Denied
                // (handled below); for Ask/AutoReview we proceed.
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
            )
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
fn block_for_approval(
    ctx: &ExecContext,
    request: &ActionRequest,
    checkpoint: bool,
    checkpoint_paths: &[PathBuf],
    pending_action: serde_json::Value,
    risk: RiskClass,
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

    let approval_id = RuntimeStore::open_default()
        .and_then(|store| {
            let approval = store.approvals().create(NewApproval {
                task_id: ctx.task_id.clone(),
                proposed_action: request.summary.clone(),
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
        )
    }

    #[test]
    fn readonly_blocks_external_tools() {
        // C1/H1/H2: the previously-bypassing tools must be denied in ReadOnly.
        let ctx = ctx(SafetyMode::ReadOnly);
        for (tool, cat) in [
            ("web_fetch", ToolCategory::Web),
            ("mcp_proxy", ToolCategory::Mcp),
            ("agent", ToolCategory::Subagent),
            ("click", ToolCategory::ComputerUse),
        ] {
            assert!(
                gate_external(&ctx, tool, cat, tool.to_string(), &serde_json::json!({})).is_some(),
                "ReadOnly must block {tool}",
            );
        }
    }

    #[test]
    fn full_access_allows_external_tools() {
        let ctx = ctx(SafetyMode::FullAccess);
        assert!(
            gate_external(
                &ctx,
                "web_fetch",
                ToolCategory::Web,
                "web_fetch".to_string(),
                &serde_json::json!({}),
            )
            .is_none()
        );
    }
}
