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

use crate::domain::{ApprovalKind, ToolOutcome};
use crate::providers::{ApprovalBroker, ApprovalDecision, allowlist_key};
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
    let mut request = ActionRequest::new(tool, category, summary);
    // Surface a concrete, content-bearing detail (the text being typed, the URL
    // being fetched, the MCP server__tool + args). Without this the Auto-mode
    // classifier and the human approval prompt see only the tool name and a
    // generic summary — so they can't actually vet *what* the action does
    // (#29, #30, #31).
    request.command = action_detail(tool, args);
    let pending = serde_json::json!({ "tool": tool, "args": args });
    match gate(ctx, request, &[], pending, false).await {
        Gate::Block(outcome) => Some(outcome),
        Gate::Proceed { .. } => None,
    }
}

/// Build a concise, content-bearing one-line detail for an external action,
/// landing in `ActionRequest.command`. This is what the Auto classifier vets
/// and the approval modal shows, so it must reflect the *content* — not just
/// the tool name. Returns `None` when there's nothing useful to add beyond the
/// summary the caller already provided.
fn action_detail(tool: &str, args: &serde_json::Value) -> Option<String> {
    /// Char-boundary-safe clamp for the preview.
    fn clip(s: &str, max: usize) -> String {
        if s.len() <= max {
            return s.to_string();
        }
        let end = s.floor_char_boundary(max);
        format!("{}…", &s[..end])
    }
    let s = |k: &str| args.get(k).and_then(|v| v.as_str());
    let i = |k: &str| args.get(k).and_then(|v| v.as_i64());
    match tool {
        "type_text" => Some(format!("type_text {:?}", clip(s("text")?, 200))),
        "press_key" => Some(format!("press_key {}", s("key").or_else(|| s("keys"))?)),
        "click" | "mouse_move" => match (i("x"), i("y")) {
            (Some(x), Some(y)) => Some(format!("{tool} ({x}, {y})")),
            _ => None,
        },
        "scroll" => {
            let dir = s("direction").unwrap_or("");
            Some(
                format!("scroll {dir} {}", i("amount").unwrap_or(0))
                    .trim()
                    .to_string(),
            )
        },
        "web_fetch" => Some(format!("web_fetch {}", clip(s("url")?, 200))),
        "mcp_proxy" => {
            let server = s("server_name").unwrap_or("?");
            let name = s("tool_name").unwrap_or("?");
            let arg_preview = args
                .get("arguments")
                .filter(|a| !a.is_null())
                .map(|a| clip(&a.to_string(), 200))
                .unwrap_or_default();
            Some(format!("mcp {server}__{name}({arg_preview})"))
        },
        "web_search" => {
            // Surface the real query text so the Auto classifier and the human
            // approval modal can catch exfiltration-via-query (`evil.com?leak=
            // <secret>`) — not just a count. Handles both the single-`query` and
            // `queries[]` shapes `web.rs` accepts (#30).
            let queries: Vec<String> = if let Some(q) = s("query") {
                vec![q.to_string()]
            } else if let Some(arr) = args.get("queries").and_then(|v| v.as_array()) {
                arr.iter()
                    .filter_map(|e| e.get("query").and_then(|v| v.as_str()))
                    .map(str::to_string)
                    .collect()
            } else {
                Vec::new()
            };
            if queries.is_empty() {
                None
            } else {
                Some(clip(&format!("web_search {}", queries.join(" | ")), 200))
            }
        },
        "agent" => {
            // The subagent prompt is model-authored and can carry injection or
            // exfiltration; surface it so the reviewer vets the real task, not
            // the short label (#31).
            Some(format!("agent: {}", clip(s("prompt")?, 200)))
        },
        _ => None,
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
            if let Some(broker) = &ctx.approval {
                // Interactive: prompt the user inline. This works for
                // replayable AND non-replayable tools — approval runs the
                // action now, so no out-of-band replay is needed (fixes the
                // old non-replayable bypass).
                inline_decision(ctx, broker, &request, risk, None).await
            } else if !replayable {
                // Headless non-replayable (web/mcp/subagent/computer_use): no
                // checkpoint/replay path, so an Ask can't be satisfied
                // out-of-band. Fail closed by default — only proceed when the
                // run explicitly opted in via `--allow-untrusted-tools`.
                if ctx.config.safety.allow_untrusted_headless_tools {
                    tracing::debug!(
                        tool = %request.tool,
                        "policy Ask on non-replayable tool; proceeding (--allow-untrusted-tools)",
                    );
                    Gate::Proceed { risk }
                } else {
                    Gate::Block(ToolOutcome::error(
                        format!(
                            "{} requires approval, but this is a headless run with no approval UI. \
                             Re-run with --allow-untrusted-tools, or use a safety mode of auto/full_access.",
                            request.summary
                        ),
                        0.0,
                    ))
                }
            } else {
                block_for_approval(
                    ctx,
                    &request,
                    checkpoint,
                    checkpoint_paths,
                    pending_action,
                    risk,
                    None,
                )
            }
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
            } else if let Some(broker) = &ctx.approval {
                // Interactive: escalate to an inline prompt carrying the reason.
                inline_decision(ctx, broker, &request, risk, Some(verdict.reason)).await
            } else if replayable {
                // Headless: escalate to a human approval the user can replay.
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
                // Non-replayable + headless: block with the reason for the model.
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

/// Interactive approval: check the session "don't ask again" allowlist, else
/// prompt the user (parking the tool task) and map their answer to a `Gate`.
/// Approval runs the action inline, so the tool's own Proceed-path checkpoint
/// covers restorability — no DB approval row / replay needed.
async fn inline_decision(
    ctx: &ExecContext,
    broker: &ApprovalBroker,
    request: &ActionRequest,
    risk: RiskClass,
    classifier_reason: Option<String>,
) -> Gate {
    let key = allowlist_key(&request.tool, request.command.as_deref());
    // An empty key marks a non-allowlistable action — always prompt, never
    // match a stored entry (#6, #31).
    if !key.is_empty() && broker.is_allowlisted(&key) {
        return Gate::Proceed { risk };
    }
    let kind = if classifier_reason.is_some() {
        ApprovalKind::Classify
    } else {
        approval_kind(request.category)
    };
    let prompt = format_approval_body(request, classifier_reason.as_deref());
    let decision = broker
        .request(
            &ctx.token,
            ctx.turn,
            ctx.call_id,
            request.tool.clone(),
            risk.as_str().to_string(),
            kind,
            prompt,
            key,
        )
        .await;
    match decision {
        ApprovalDecision::Approve | ApprovalDecision::ApproveAlways => Gate::Proceed { risk },
        ApprovalDecision::Deny => Gate::Block(ToolOutcome::error(
            format!("{} — denied by you", request.summary),
            0.0,
        )),
    }
}

/// Build the modal body: the concrete command/path being run, plus any
/// Auto-review reason. Kept here so the render layer stays dumb.
fn format_approval_body(request: &ActionRequest, classifier_reason: Option<&str>) -> String {
    use crate::runtime::ToolCategory as C;
    let mut body = if let Some(cmd) = &request.command {
        // A `$ ` prefix reads as "shell command"; only use it for actual shell
        // categories. Computer-use / MCP details (`type_text "…"`, `mcp s__t(…)`)
        // render verbatim so the prompt isn't misleading (#30, #31).
        match request.category {
            C::Shell | C::Git | C::Process => format!("$ {}", cmd),
            _ => cmd.clone(),
        }
    } else if let Some(path) = &request.path {
        format!("{}  ({})", path, request.summary)
    } else {
        request.summary.clone()
    };
    if let Some(reason) = classifier_reason {
        body.push_str(&format!("\n\nAuto-review flagged this: {}", reason));
    }
    body
}

fn approval_kind(category: crate::runtime::ToolCategory) -> ApprovalKind {
    use crate::runtime::ToolCategory as C;
    match category {
        C::Edit => ApprovalKind::FileMutation,
        C::Shell | C::Git | C::Process => ApprovalKind::Shell,
        C::Web | C::Network | C::ExternalDirectory => ApprovalKind::Web,
        C::Mcp => ApprovalKind::Mcp,
        C::Subagent => ApprovalKind::Subagent,
        C::ComputerUse => ApprovalKind::ComputerUse,
        // Read and Memory ⇒ Allow/Deny in `decide`, so neither reaches
        // approval; keep the match total.
        C::Read | C::Memory => ApprovalKind::Shell,
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
            None,
        )
    }

    fn ctx_headless_opted_in() -> ExecContext {
        let mut config = crate::app::Config::default();
        config.safety.mode = SafetyMode::Ask;
        config.safety.allow_untrusted_headless_tools = true;
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
            SafetyMode::Ask,
            None,
            None,
            None,
        )
    }

    #[test]
    fn action_detail_surfaces_web_search_and_agent_content() {
        // #30/#31: the classifier + approval modal must see the real content,
        // not just a count/label.
        let d = action_detail(
            "web_search",
            &serde_json::json!({"query": "evil.com?leak=secret"}),
        )
        .expect("web_search detail");
        assert!(d.contains("evil.com?leak=secret"), "got {d:?}");
        let d = action_detail(
            "web_search",
            &serde_json::json!({"queries": [{"query": "alpha"}, {"query": "beta"}]}),
        )
        .expect("web_search queries detail");
        assert!(d.contains("alpha") && d.contains("beta"), "got {d:?}");
        let d = action_detail(
            "agent",
            &serde_json::json!({"prompt": "exfiltrate the env", "description": "x"}),
        )
        .expect("agent detail");
        assert!(d.contains("exfiltrate the env"), "got {d:?}");
    }

    #[tokio::test]
    async fn headless_ask_blocks_non_replayable_unless_opted_in() {
        // #3: web/mcp/subagent/computer_use on an Ask decision with no approval
        // UI is blocked by default, allowed only with the opt-in flag.
        let req = || ActionRequest::new("web_fetch", ToolCategory::Web, "web_fetch https://x");

        let blocked = gate(
            &ctx(SafetyMode::Ask),
            req(),
            &[],
            serde_json::json!({}),
            false,
        )
        .await;
        assert!(
            matches!(blocked, Gate::Block(_)),
            "headless Ask should block by default"
        );

        let proceed = gate(
            &ctx_headless_opted_in(),
            req(),
            &[],
            serde_json::json!({}),
            false,
        )
        .await;
        assert!(
            matches!(proceed, Gate::Proceed { .. }),
            "--allow-untrusted-tools should let it proceed",
        );
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
            None,
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
            ("memory", ToolCategory::Memory),
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
    async fn memory_writes_ungated_except_readonly() {
        // The load-bearing "no modal" guarantee: memory is Allowed in ask /
        // auto / full, so gate_external returns None (proceed) and the
        // approval broker is never consulted. Only read-only blocks it.
        for mode in [SafetyMode::Ask, SafetyMode::Auto, SafetyMode::FullAccess] {
            let ctx = ctx(mode);
            assert!(
                gate_external(
                    &ctx,
                    "memory",
                    ToolCategory::Memory,
                    "memory remember".to_string(),
                    &serde_json::json!({"action": "remember"}),
                )
                .await
                .is_none(),
                "memory must proceed without approval in {mode:?}",
            );
        }
        let ctx = ctx(SafetyMode::ReadOnly);
        assert!(
            gate_external(
                &ctx,
                "memory",
                ToolCategory::Memory,
                "memory remember".to_string(),
                &serde_json::json!({"action": "remember"}),
            )
            .await
            .is_some(),
            "read-only must block memory writes",
        );
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

    /// Build an `Ask`-mode ctx with an inline-approval broker bound.
    fn ctx_with_broker(broker: crate::providers::ApprovalBroker) -> ExecContext {
        let mut config = crate::app::Config::default();
        config.safety.mode = SafetyMode::Ask;
        let (tx, _rx) = tokio::sync::mpsc::channel::<ProgressEvent>(4);
        ExecContext::new(
            tokio_util::sync::CancellationToken::new(),
            tx,
            ToolCallId(7),
            TurnId(1),
            PathBuf::from("."),
            Arc::new(config),
            String::new(),
            None,
            SafetyMode::Ask,
            None,
            None,
            Some(broker),
        )
    }

    fn shell_request(cmd: &str) -> ActionRequest {
        let mut req = ActionRequest::new("execute_command", ToolCategory::Shell, cmd);
        req.command = Some(cmd.to_string());
        req
    }

    #[tokio::test]
    async fn inline_ask_approve_proceeds() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<crate::domain::Msg>(8);
        let broker = crate::providers::ApprovalBroker::new(tx);
        let ctx = ctx_with_broker(broker.clone());
        let handle = tokio::spawn(async move {
            gate(
                &ctx,
                shell_request("npm test"),
                &[],
                serde_json::json!({}),
                true,
            )
            .await
        });
        // Observe the prompt, then approve it.
        let call_id = match rx.recv().await.expect("approval requested") {
            crate::domain::Msg::ApprovalRequested { call_id, .. } => call_id,
            other => panic!("expected ApprovalRequested, got {other:?}"),
        };
        broker.resolve(call_id, crate::providers::ApprovalDecision::Approve);
        assert!(matches!(handle.await.unwrap(), Gate::Proceed { .. }));
    }

    #[tokio::test]
    async fn inline_ask_deny_blocks() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<crate::domain::Msg>(8);
        let broker = crate::providers::ApprovalBroker::new(tx);
        let ctx = ctx_with_broker(broker.clone());
        let handle = tokio::spawn(async move {
            gate(
                &ctx,
                shell_request("rm -rf node_modules"),
                &[],
                serde_json::json!({}),
                true,
            )
            .await
        });
        let call_id = match rx.recv().await.expect("approval requested") {
            crate::domain::Msg::ApprovalRequested { call_id, .. } => call_id,
            other => panic!("expected ApprovalRequested, got {other:?}"),
        };
        broker.resolve(call_id, crate::providers::ApprovalDecision::Deny);
        assert!(matches!(handle.await.unwrap(), Gate::Block(_)));
    }

    #[tokio::test]
    async fn inline_allowlisted_skips_prompt() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<crate::domain::Msg>(8);
        let broker = crate::providers::ApprovalBroker::new(tx);
        // Approve-always once so the key is allowlisted, then the SAME command
        // must proceed WITHOUT emitting another prompt. (#6: execute_command
        // keys on the full normalized command, so only an identical command is
        // cleared — a different-argument command re-prompts; that distinction is
        // covered by the `allowlist_key` unit tests.)
        let ctx1 = ctx_with_broker(broker.clone());
        let b1 = broker.clone();
        let h1 = tokio::spawn(async move {
            gate(
                &ctx1,
                shell_request("npm run build"),
                &[],
                serde_json::json!({}),
                true,
            )
            .await
        });
        let id = match rx.recv().await.expect("first prompt") {
            crate::domain::Msg::ApprovalRequested { call_id, .. } => call_id,
            other => panic!("got {other:?}"),
        };
        b1.resolve(id, crate::providers::ApprovalDecision::ApproveAlways);
        assert!(matches!(h1.await.unwrap(), Gate::Proceed { .. }));

        let ctx2 = ctx_with_broker(broker.clone());
        let g2 = gate(
            &ctx2,
            shell_request("npm run build"),
            &[],
            serde_json::json!({}),
            true,
        )
        .await;
        assert!(
            matches!(g2, Gate::Proceed { .. }),
            "the identical allowlisted command should skip the prompt"
        );
        assert!(rx.try_recv().is_err(), "no second prompt should be sent");
    }
}
