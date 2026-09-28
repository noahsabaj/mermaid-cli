//! Central safety-policy gate shared by every tool that can mutate the
//! workspace, touch the network, drive the desktop, or spawn work.
//!
//! Before v0.7.x the `PolicyEngine` was only consulted by `execute_command`
//! and the filesystem mutators; `web_*`, `mcp`, and `subagent` ran their
//! bodies with no policy check at all, so
//! `SafetyMode::ReadOnly` silently failed to block them. This module is the
//! single choke point: every dangerous tool builds an [`ActionRequest`] and
//! calls [`gate`] before acting.
//!
//! `replayable` distinguishes the two enforcement shapes:
//!
//! - **Replayable tools** (`execute_command`, file mutators): an `Ask` (or an
//!   escalated Auto-mode `Classify`) decision creates a checkpoint + an
//!   approval row and BLOCKS, returning an "approval required" outcome. The
//!   action is later re-run out-of-band by [`mermaid_runtime::approve_and_replay`].
//! - **Non-replayable tools** (`web_*`, `mcp`, `subagent`):
//!   there is no checkpoint/replay path, so an `Ask` decision resolves
//!   inline when an approval broker is bound and otherwise fails closed
//!   unless the run opted in via `--allow-untrusted-tools`. `ReadOnly` denies
//!   mutations and control actions, permits inherited-safety subagent spawn,
//!   and requires one-shot approval for externally observable Web egress.
//!
//! `Auto` mode resolves a `PolicyDecision::Classify` here by awaiting the
//! injected `AutoClassifier` (in `ctx.classifier`): aligned ⇒ proceed,
//! otherwise escalate — a replayable tool to a human approval, a non-replayable
//! tool to a block (it can't be replayed). A missing classifier or any vet
//! failure fails safe to escalate. This is why `gate` is `async`.

use std::path::PathBuf;

use crate::providers::{ApprovalBroker, ApprovalDecision, allowlist_key, is_domain_allowed};
use mermaid_domain::{ApprovalKind, ToolOutcome};
use mermaid_runtime::{
    ActionRequest, NewApproval, PolicyDecision, PolicyEngine, RiskClass,
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

/// Convenience for non-replayable tools (`web_*`, `mcp`, `subagent`):
/// consult the policy and return `Some(outcome)` when the
/// action is blocked (e.g. `ReadOnly`/`Deny` override), or `None` to proceed.
/// These tools have no checkpoint/replay path: an `Ask` decision resolves
/// inline when an approval broker is bound and otherwise fails closed unless
/// `--allow-untrusted-tools`. `ReadOnly` blocks mutations/control actions,
/// allows inherited-safety subagent spawn, and asks once for Web egress. Call
/// this at the very top of `execute()`.
pub async fn gate_external(
    ctx: &ExecContext,
    tool: &'static str,
    category: mermaid_runtime::ToolCategory,
    summary: String,
    args: &serde_json::Value,
) -> Option<ToolOutcome> {
    gate_external_inner(ctx, tool, category, summary, args, false).await
}

/// MCP variant of [`gate_external`]: carries the server-advertised
/// `readOnlyHint` so the policy's external-writes floor can tell read-shaped
/// calls from write-shaped ones. Pass `false` when the hint is unknown.
pub async fn gate_external_mcp(
    ctx: &ExecContext,
    summary: String,
    args: &serde_json::Value,
    read_only_hint: bool,
) -> Option<ToolOutcome> {
    gate_external_inner(
        ctx,
        "mcp_proxy",
        mermaid_runtime::ToolCategory::Mcp,
        summary,
        args,
        read_only_hint,
    )
    .await
}

async fn gate_external_inner(
    ctx: &ExecContext,
    tool: &'static str,
    category: mermaid_runtime::ToolCategory,
    summary: String,
    args: &serde_json::Value,
    mcp_read_only_hint: bool,
) -> Option<ToolOutcome> {
    if matches!(
        category,
        mermaid_runtime::ToolCategory::Web | mermaid_runtime::ToolCategory::Network
    ) && matches!(
        ctx.config.safety.network,
        mermaid_domain::NetworkPolicy::Deny
    ) {
        return Some(ToolOutcome::error(
            format!(
                "{tool} blocked because network access is disabled (safety.network = \"deny\" / --no-network)"
            ),
            None,
        ));
    }
    let mut request = ActionRequest::new(tool, category, summary);
    // Surface a concrete, content-bearing detail (the text being typed, the URL
    // being fetched, the MCP server__tool + args). Without this the Auto-mode
    // classifier and the human approval prompt see only the tool name and a
    // generic summary — so they can't actually vet *what* the action does
    // (#29, #30, #31).
    request.command = action_detail(tool, args);
    request.arguments = Some(args.clone());
    request.mcp_read_only_hint = mcp_read_only_hint;
    let pending = serde_json::json!({ "tool": tool, "args": args });
    // `scratch_contained` is always false here: external actions (network,
    // desktop, MCP, subagents) act OUTSIDE the filesystem, so scratchpad
    // containment can never be proven for them.
    match gate(ctx, request, &[], pending, false, false).await {
        Gate::Block(outcome) => Some(outcome),
        Gate::Proceed { .. } => None,
    }
}

/// Build the complete content-bearing detail for policy matching and the Auto
/// classifier. This value is deliberately not clipped: presentation limits are
/// applied only when rendering an approval modal.
fn action_detail(tool: &str, args: &serde_json::Value) -> Option<String> {
    let s = |k: &str| args.get(k).and_then(|v| v.as_str());
    let i = |k: &str| args.get(k).and_then(|v| v.as_i64());
    match tool {
        "type_text" => Some(format!("type_text {:?}", s("text")?)),
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
        "web_fetch" => Some(format!("web_fetch {}", s("url")?)),
        // `execute_command`'s `open_url`: the URL is the whole action,
        // and exfil-via-URL is the reason it is gated at all, so the
        // classifier and the approval modal must see it in full.
        "open_url" => Some(format!("open_url {}", s("url")?)),
        "mcp_proxy" => {
            let server = s("server_name").unwrap_or("?");
            let name = s("tool_name").unwrap_or("?");
            let arg_preview = args
                .get("arguments")
                .filter(|a| !a.is_null())
                .map(serde_json::Value::to_string)
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
                Some(format!("web_search {}", queries.join(" | ")))
            }
        },
        "agent" => {
            // The subagent prompt is model-authored and can carry injection or
            // exfiltration; surface it so the reviewer vets the real task, not
            // the short label (#31).
            Some(format!("agent: {}", s("prompt")?))
        },
        _ => None,
    }
}

/// Consult the safety policy for `request`. See the module docs for the
/// `replayable` semantics.
///
/// `scratch_contained` is true when the caller has PROVEN the action touches
/// only the session scratchpad (`ResolvedInRoot::containment` for the file
/// mutators, `command_provably_in_scratch` for the exec tool). Scratch files
/// are session-private and ephemeral, so — like durable memory in
/// `PolicyEngine::decide` — an `Ask`/`Classify` on an eligible risk class is
/// downgraded to proceed. The downgrade never touches `Deny` (a user `Deny`
/// override, read-only mode, and the destructive hard-deny all still block)
/// and never applies to risks that act beyond the filesystem
/// ([`scratch_downgrade_eligible`]).
#[expect(
    clippy::too_many_lines,
    reason = "the precedence ladder of the safety gate: engine decision, the two read-only web \
     softenings, the scratchpad downgrade, then one arm per PolicyDecision; each \
     softening is only correct relative to the ones above it, so the order is the security \
     property and it has to be read whole"
)]
pub async fn gate(
    ctx: &ExecContext,
    request: ActionRequest,
    checkpoint_paths: &[PathBuf],
    pending_action: serde_json::Value,
    replayable: bool,
    scratch_contained: bool,
) -> Gate {
    // Build from the LIVE session mode (Shift+Tab / `/safety` take effect
    // immediately), not the static config snapshot.
    let decision = PolicyEngine::new(ctx.safety_mode)
        .with_overrides(ctx.config.safety.overrides.clone())
        .with_external_writes(ctx.config.safety.external_writes)
        .with_system_installs(ctx.config.safety.system_installs)
        .decide(&request);

    // Explicit user/session opt-in restores unattended public-web reads in
    // ReadOnly. It may only soften the mode-generated Ask: global network deny
    // is enforced before this function, while policy Deny decisions remain
    // untouched. Project config cannot set this flag.
    let decision = match decision {
        PolicyDecision::Ask { risk, .. }
            if ctx.safety_mode == mermaid_runtime::SafetyMode::ReadOnly
                && request.category == mermaid_runtime::ToolCategory::Web
                && ctx.config.safety.allow_readonly_web =>
        {
            PolicyDecision::Allow {
                risk,
                checkpoint: false,
            }
        },
        // Allowed domains in [web] config allowlist matching hosts without prompts/classifier:
        PolicyDecision::Ask { risk, .. } | PolicyDecision::Classify { risk, .. }
            if request.tool == "web_fetch"
                && request
                    .command
                    .as_deref()
                    .is_some_and(|cmd| is_domain_allowed(&ctx.config.web.allowed_domains, cmd)) =>
        {
            PolicyDecision::Allow {
                risk,
                checkpoint: false,
            }
        },
        other => other,
    };

    // Scratchpad downgrade: an Ask/Classify on a proven scratch-only action
    // proceeds without a prompt. Deny is deliberately not matched — it falls
    // through to the arm below and blocks, so read-only mode (a Deny) keeps
    // blocking scratch mutations.
    if scratch_contained
        && let PolicyDecision::Ask { risk, .. } | PolicyDecision::Classify { risk, .. } = decision
        && scratch_downgrade_eligible(risk)
    {
        return Gate::Proceed { risk };
    }

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
                // Headless non-replayable (web/mcp/subagent): no
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
                    // Read-only web has its own, narrower remedy — name it,
                    // or the model's operator is left choosing between a
                    // blanket trust flag and giving up read-only entirely.
                    let readonly_web_hint = if request.category
                        == mermaid_runtime::ToolCategory::Web
                        && ctx.safety_mode == mermaid_runtime::SafetyMode::ReadOnly
                    {
                        " In read_only, [safety] allow_readonly_web = true permits \
                         unattended public-web reads."
                    } else {
                        ""
                    };
                    Gate::Block(ToolOutcome::error(
                        format!(
                            "{} requires approval, but this is a headless run with no approval UI. \
                             Re-run with --allow-untrusted-tools, or use a safety mode of auto/full_access.{readonly_web_hint}",
                            request.summary
                        ),
                        None,
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
                        arguments: request.arguments.clone(),
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
                    None,
                ))
            }
        },
        PolicyDecision::Deny { reason, .. } => Gate::Block(ToolOutcome::error(
            format!("{} blocked by policy: {}", request.summary, reason),
            None,
        )),
    }
}

/// Risk classes whose `Ask`/`Classify` may be downgraded to proceed when the
/// action is proven scratch-contained. File and shell mutations confined to
/// the scratchpad can only touch session-private throwaway files; everything
/// stronger acts beyond the filesystem and keeps its normal gating:
/// `Network` can exfiltrate regardless of cwd, `Process`/`ExternalAccess`
/// control things outside any directory, and `Destructive` is hard-denied
/// upstream anyway.
fn scratch_downgrade_eligible(risk: RiskClass) -> bool {
    matches!(
        risk,
        RiskClass::ReadOnly
            | RiskClass::LowMutation
            | RiskClass::FileMutation
            | RiskClass::ShellMutation
    )
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
    let external_path = (request.category == mermaid_runtime::ToolCategory::ExternalDirectory)
        .then_some(request.path.as_deref())
        .flatten();
    let key = allowlist_key(&request.tool, request.command.as_deref(), external_path);
    // An empty key marks a non-allowlistable action — always prompt, never
    // match a stored entry (#6, #31).
    if !key.is_empty() && broker.is_allowlisted(&key) {
        return Gate::Proceed { risk };
    }
    let kind = if classifier_reason.is_some() {
        ApprovalKind::Classify
    } else {
        ApprovalKind::from(request.category)
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
            None,
        )),
    }
}

/// Build the modal body: the concrete command/path being run, plus any
/// Auto-review reason. Kept here so the render layer stays dumb.
fn format_approval_body(request: &ActionRequest, classifier_reason: Option<&str>) -> String {
    fn clip_preview(value: &str) -> String {
        const MAX_BYTES: usize = 200;
        if value.len() <= MAX_BYTES {
            return value.to_string();
        }
        let end = value.floor_char_boundary(MAX_BYTES);
        format!("{}…", &value[..end])
    }

    use mermaid_runtime::ToolCategory as C;
    let redacted_detail = request.arguments.as_ref().and_then(|arguments| {
        let mut safe = arguments.clone();
        mermaid_model::utils::redact_json(&mut safe);
        action_detail(&request.tool, &safe)
    });
    let modal_detail = redacted_detail.as_ref().or(request.command.as_ref());
    let mut body = if let Some(cmd) = modal_detail {
        // A prompt sigil reads as "shell command"; only use it for actual
        // shell categories. MCP details (`mcp s__t(…)`) render verbatim
        // so the prompt isn't misleading
        // (#30, #31). The sigil is the HOST shell's (`$ ` / `PS> `): telling
        // the reader they are approving a POSIX command when PowerShell will
        // run it is the same lie the `Bash(...)` transcript label told.
        match request.category {
            C::Shell | C::Git | C::Process => format!(
                "{}{cmd}",
                mermaid_runtime::HostShell::current().prompt_sigil()
            ),
            _ => clip_preview(cmd),
        }
    } else if let Some(path) = &request.path {
        format!("{}  ({})", path, request.summary)
    } else {
        match request.category {
            C::Shell | C::Git | C::Process => request.summary.clone(),
            _ => clip_preview(&request.summary),
        }
    };
    if let Some(reason) = classifier_reason {
        body.push_str(&format!("\n\nAuto-review flagged this: {reason}"));
    }
    body
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
    // When the escalation came from the Auto-mode classifier, its reason —
    // recorded on the approval so the user sees *why* it was flagged.
    classifier_reason: Option<String>,
) -> Gate {
    let checkpoint_id = if checkpoint && ctx.config.safety.checkpoint_on_mutation {
        match create_checkpoint_for_task(
            &ctx.workdir,
            checkpoint_paths,
            Some(pending_action.clone()),
            ctx.checkpoint_origin(),
        ) {
            Ok(manifest) => Some(manifest.id),
            Err(error) => {
                return Gate::Block(ToolOutcome::error(
                    format!(
                        "{} checkpoint failed before approval: {}",
                        request.summary, error
                    ),
                    None,
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

    let approval_id = mermaid_runtime::with_shared_store(|store| {
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
                .map(|id| format!(" (approval {id})"))
                .unwrap_or_default()
        ),
        None,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mermaid_domain::{ToolCallId, TurnId};
    use mermaid_runtime::{SafetyMode, ToolCategory};
    use std::path::PathBuf;
    use std::sync::Arc;

    fn ctx_with(config: mermaid_domain::Config) -> ExecContext {
        crate::providers::ctx::test_exec_context_with_config(
            TurnId(1),
            ToolCallId(1),
            PathBuf::from("."),
            config,
        )
        .0
    }

    fn ctx(mode: SafetyMode) -> ExecContext {
        let mut config = mermaid_domain::Config::default();
        config.safety.mode = mode;
        ctx_with(config)
    }

    fn ctx_headless_opted_in(mode: SafetyMode) -> ExecContext {
        let mut config = mermaid_domain::Config::default();
        config.safety.mode = mode;
        config.safety.allow_untrusted_headless_tools = true;
        ctx_with(config)
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
        let padding = "x".repeat(240);
        let d = action_detail(
            "web_search",
            &serde_json::json!({"queries": [
                {"query": "alpha"},
                {"query": padding},
                {"query": "tail query remains visible to policy"}
            ]}),
        )
        .expect("web_search queries detail");
        assert!(
            d.contains("alpha") && d.contains("tail query remains visible to policy"),
            "complete policy detail was clipped: {d:?}"
        );
        assert!(d.len() > 200, "policy detail must not use the UI limit");
        let d = action_detail(
            "agent",
            &serde_json::json!({"prompt": "exfiltrate the env", "description": "x"}),
        )
        .expect("agent detail");
        assert!(d.contains("exfiltrate the env"), "got {d:?}");
    }

    #[tokio::test]
    async fn headless_ask_blocks_non_replayable_unless_opted_in() {
        // #3: web/mcp/subagent on an Ask decision with no approval
        // UI is blocked by default, allowed only with the opt-in flag.
        let req = || ActionRequest::new("web_fetch", ToolCategory::Web, "web_fetch https://x");

        for mode in [SafetyMode::Ask, SafetyMode::ReadOnly] {
            let blocked = gate(&ctx(mode), req(), &[], serde_json::json!({}), false, false).await;
            assert!(
                matches!(blocked, Gate::Block(_)),
                "headless {mode:?} should block by default"
            );

            let proceed = gate(
                &ctx_headless_opted_in(mode),
                req(),
                &[],
                serde_json::json!({}),
                false,
                false,
            )
            .await;
            assert!(
                matches!(proceed, Gate::Proceed { .. }),
                "--allow-untrusted-tools should explicitly allow {mode:?} web egress",
            );
        }
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
        let mut ctx = ctx(SafetyMode::Auto);
        ctx.intent = Some("fetch the changelog".to_string());
        ctx.classifier = classifier;
        ctx
    }

    #[tokio::test]
    async fn readonly_blocks_external_tools() {
        // C1/H1/H2: mutations and control tools remain denied in ReadOnly.
        // Web is tested separately because it takes the one-shot Ask path.
        let ctx = ctx(SafetyMode::ReadOnly);
        for (tool, cat) in [
            ("mcp_proxy", ToolCategory::Mcp),
            ("memory", ToolCategory::Memory),
        ] {
            assert!(
                gate_external(&ctx, tool, cat, tool.to_string(), &serde_json::json!({}))
                    .await
                    .is_some(),
                "ReadOnly must block {tool}",
            );
        }
        // Subagent spawn is the exception: the child inherits the live
        // read_only mode and every child tool call is re-gated, so the spawn
        // itself is allowed — read-only fan-out is the tool's core use.
        assert!(
            gate_external(
                &ctx,
                "agent",
                ToolCategory::Subagent,
                "subagent: explore".to_string(),
                &serde_json::json!({"prompt": "map the crates"}),
            )
            .await
            .is_none(),
            "ReadOnly must allow subagent spawn",
        );
    }

    #[tokio::test]
    async fn readonly_web_egress_fails_closed_without_approval_ui() {
        let ask = ctx(SafetyMode::Ask);
        let ctx = ctx(SafetyMode::ReadOnly);
        for (tool, summary) in [
            ("web_search", "web_search rust release notes"),
            ("web_fetch", "web_fetch https://example.com/docs"),
        ] {
            let blocked = gate_external(
                &ctx,
                tool,
                ToolCategory::Web,
                summary.to_string(),
                &serde_json::json!({}),
            )
            .await;
            assert!(
                blocked.is_some(),
                "ReadOnly must require approval for {tool}"
            );
            // The denial must name read_only's own remedy, not only the
            // blanket trust flag (which trades away far more than web reads).
            let outcome = blocked.expect("asserted Some above");
            let msg = outcome.error_message().unwrap_or_default();
            assert!(msg.contains("allow_readonly_web"), "{tool}: {msg}");
        }
        // Matched pair: the same denial outside read_only must NOT advertise
        // the read_only-only flag.
        let blocked = gate_external(
            &ask,
            "web_fetch",
            ToolCategory::Web,
            "web_fetch https://example.com/docs".to_string(),
            &serde_json::json!({}),
        )
        .await
        .expect("Ask must block headless web egress");
        let msg = blocked.error_message().unwrap_or_default();
        assert!(!msg.contains("allow_readonly_web"), "{msg}");
    }

    #[tokio::test]
    async fn readonly_web_explicit_user_opt_in_proceeds() {
        let mut context = ctx(SafetyMode::ReadOnly);
        Arc::make_mut(&mut context.config).safety.allow_readonly_web = true;
        assert!(
            gate_external(
                &context,
                "web_fetch",
                ToolCategory::Web,
                "web_fetch example".to_string(),
                &serde_json::json!({"url": "https://example.com"}),
            )
            .await
            .is_none(),
            "explicit user/session opt-in should allow ReadOnly web egress"
        );
    }

    #[tokio::test]
    async fn global_network_deny_blocks_web_even_in_full_access() {
        let mut context = ctx(SafetyMode::FullAccess);
        let safety = &mut Arc::make_mut(&mut context.config).safety;
        safety.network = mermaid_domain::NetworkPolicy::Deny;
        safety.allow_readonly_web = true;
        for tool in ["web_fetch", "web_search"] {
            let blocked = gate_external(
                &context,
                tool,
                ToolCategory::Web,
                tool.to_string(),
                &serde_json::json!({"url": "https://example.com"}),
            )
            .await;
            assert!(blocked.is_some(), "network deny must block {tool}");
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

    fn ctx_with_broker_mode(
        mode: SafetyMode,
        broker: crate::providers::ApprovalBroker,
    ) -> ExecContext {
        let mut ctx = ctx(mode);
        ctx.call_id = ToolCallId(7);
        ctx.approval = Some(broker);
        ctx
    }

    /// Build an `Ask`-mode ctx with an inline-approval broker bound.
    fn ctx_with_broker(broker: crate::providers::ApprovalBroker) -> ExecContext {
        ctx_with_broker_mode(SafetyMode::Ask, broker)
    }

    #[tokio::test]
    async fn readonly_web_uses_domain_allowlist_approval() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<mermaid_domain::Msg>(8);
        let broker = crate::providers::ApprovalBroker::new(tx);
        let context = ctx_with_broker_mode(SafetyMode::ReadOnly, broker.clone());
        let handle = tokio::spawn(async move {
            gate_external(
                &context,
                "web_fetch",
                ToolCategory::Web,
                "web_fetch example".to_string(),
                &serde_json::json!({"url": "https://example.com"}),
            )
            .await
        });

        let (call_id, allowlist_scope) = match rx.recv().await.expect("approval requested") {
            mermaid_domain::Msg::ApprovalRequested {
                call_id,
                allowlist_scope,
                ..
            } => (call_id, allowlist_scope),
            other => panic!("expected ApprovalRequested, got {other:?}"),
        };
        assert_eq!(
            allowlist_scope, "web_fetch:example.com",
            "web approval must expose domain-level approve-always"
        );
        broker.resolve(call_id, crate::providers::ApprovalDecision::ApproveAlways);
        assert!(
            handle.await.unwrap().is_none(),
            "one approved request should proceed"
        );
        assert!(broker.is_allowlisted("web_fetch:example.com"));
    }

    #[test]
    fn external_policy_detail_is_complete_but_modal_preview_is_bounded() {
        let tail = "tail-visible-only-to-policy";
        let url = format!("https://example.com/{}{}", "x".repeat(240), tail);
        let arguments = serde_json::json!({"url": url});
        let mut request = ActionRequest::new("web_fetch", ToolCategory::Web, "web_fetch");
        request.command = action_detail("web_fetch", &arguments);
        request.arguments = Some(arguments);

        assert!(request.command.as_deref().is_some_and(|d| d.contains(tail)));
        let modal = format_approval_body(&request, None);
        assert!(!modal.contains(tail), "modal should contain only a preview");
        assert!(modal.ends_with('…'));
    }

    #[test]
    fn web_approval_modal_sanitizes_url_credentials_and_fragment() {
        let arguments = serde_json::json!({
            "url": "https://alice:password123@example.com/path?token=opaque-secret-value#private-fragment"
        });
        let mut request = ActionRequest::new("web_fetch", ToolCategory::Web, "web_fetch");
        request.command = action_detail("web_fetch", &arguments);
        request.arguments = Some(arguments);

        let modal = format_approval_body(&request, None);
        assert!(!modal.contains("alice"));
        assert!(!modal.contains("password123"));
        assert!(!modal.contains("opaque-secret-value"));
        assert!(!modal.contains("private-fragment"));
        assert!(modal.contains("example.com/path?token="));
    }

    fn shell_request(cmd: &str) -> ActionRequest {
        let mut req = ActionRequest::new("execute_command", ToolCategory::Shell, cmd);
        req.command = Some(cmd.to_string());
        req
    }

    fn edit_request(path: &str) -> ActionRequest {
        let mut req = ActionRequest::new(
            "write_file",
            ToolCategory::Edit,
            format!("write_file {path}"),
        );
        req.path = Some(path.to_string());
        req
    }

    #[tokio::test]
    async fn inline_ask_approve_proceeds() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<mermaid_domain::Msg>(8);
        let broker = crate::providers::ApprovalBroker::new(tx);
        let ctx = ctx_with_broker(broker.clone());
        let handle = tokio::spawn(async move {
            gate(
                &ctx,
                shell_request("npm test"),
                &[],
                serde_json::json!({}),
                true,
                false,
            )
            .await
        });
        // Observe the prompt, then approve it.
        let call_id = match rx.recv().await.expect("approval requested") {
            mermaid_domain::Msg::ApprovalRequested { call_id, .. } => call_id,
            other => panic!("expected ApprovalRequested, got {other:?}"),
        };
        broker.resolve(call_id, crate::providers::ApprovalDecision::Approve);
        assert!(matches!(handle.await.unwrap(), Gate::Proceed { .. }));
    }

    #[tokio::test]
    async fn inline_ask_deny_blocks() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<mermaid_domain::Msg>(8);
        let broker = crate::providers::ApprovalBroker::new(tx);
        let ctx = ctx_with_broker(broker.clone());
        let handle = tokio::spawn(async move {
            gate(
                &ctx,
                shell_request("rm -rf node_modules"),
                &[],
                serde_json::json!({}),
                true,
                false,
            )
            .await
        });
        let call_id = match rx.recv().await.expect("approval requested") {
            mermaid_domain::Msg::ApprovalRequested { call_id, .. } => call_id,
            other => panic!("expected ApprovalRequested, got {other:?}"),
        };
        broker.resolve(call_id, crate::providers::ApprovalDecision::Deny);
        assert!(matches!(handle.await.unwrap(), Gate::Block(_)));
    }

    #[tokio::test]
    async fn inline_allowlisted_skips_prompt() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<mermaid_domain::Msg>(8);
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
                false,
            )
            .await
        });
        let id = match rx.recv().await.expect("first prompt") {
            mermaid_domain::Msg::ApprovalRequested { call_id, .. } => call_id,
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
            false,
        )
        .await;
        assert!(
            matches!(g2, Gate::Proceed { .. }),
            "the identical allowlisted command should skip the prompt"
        );
        assert!(rx.try_recv().is_err(), "no second prompt should be sent");
    }

    #[tokio::test]
    async fn scratch_containment_downgrades_eligible_asks() {
        // Ask mode with NO broker: an un-downgraded replayable Ask would go to
        // `block_for_approval`, so a Proceed here proves the downgrade fired.
        let ctx = ctx(SafetyMode::Ask);
        let g = gate(
            &ctx,
            edit_request("/scratch/notes.txt"),
            &[],
            serde_json::json!({}),
            true,
            true,
        )
        .await;
        assert!(
            matches!(g, Gate::Proceed { .. }),
            "scratch-contained file mutation must proceed in Ask mode",
        );
        let g = gate(
            &ctx,
            shell_request("mkdir out"),
            &[],
            serde_json::json!({}),
            true,
            true,
        )
        .await;
        assert!(
            matches!(g, Gate::Proceed { .. }),
            "scratch-contained shell mutation must proceed in Ask mode",
        );

        // Auto mode with NO classifier: an un-downgraded Classify fails safe
        // to escalate, so a Proceed proves the Classify downgrade too.
        let ctx = ctx_auto(None);
        let g = gate(
            &ctx,
            shell_request("mkdir out"),
            &[],
            serde_json::json!({}),
            true,
            true,
        )
        .await;
        assert!(
            matches!(g, Gate::Proceed { .. }),
            "scratch-contained Classify must proceed without a classifier",
        );
    }

    #[tokio::test]
    async fn scratch_containment_never_downgrades_destructive() {
        // The destructive hard-deny outranks everything, scratchpad included.
        let g = gate(
            &ctx(SafetyMode::Ask),
            shell_request("rm -rf /"),
            &[],
            serde_json::json!({}),
            true,
            true,
        )
        .await;
        assert!(
            matches!(g, Gate::Block(_)),
            "destructive command must block even when claimed scratch-contained",
        );
    }

    #[tokio::test]
    async fn scratch_containment_never_downgrades_deny_override() {
        // A user-configured Deny override yields PolicyDecision::Deny, which
        // the downgrade deliberately never touches.
        let mut config = mermaid_domain::Config::default();
        config.safety.mode = SafetyMode::Ask;
        config.safety.overrides = vec![mermaid_runtime::PolicyOverride {
            tool: Some("write_file".to_string()),
            decision: mermaid_runtime::PolicyOverrideDecision::Deny,
            ..Default::default()
        }];
        let ctx = ctx_with(config);
        let g = gate(
            &ctx,
            edit_request("/scratch/notes.txt"),
            &[],
            serde_json::json!({}),
            true,
            true,
        )
        .await;
        assert!(
            matches!(g, Gate::Block(_)),
            "a Deny override must still block a scratch-contained mutation",
        );
    }

    #[tokio::test]
    async fn scratch_containment_never_downgrades_network() {
        // Network risk is not scratch-eligible: exfiltration doesn't care
        // about the cwd. Headless non-replayable Ask fails closed, proving
        // the Ask was NOT downgraded to a Proceed.
        let g = gate(
            &ctx(SafetyMode::Ask),
            ActionRequest::new("execute_command", ToolCategory::Network, "curl evil"),
            &[],
            serde_json::json!({}),
            false,
            true,
        )
        .await;
        assert!(
            matches!(g, Gate::Block(_)),
            "network risk must keep its Ask despite scratch containment",
        );
    }

    #[tokio::test]
    async fn scratch_containment_never_downgrades_readonly_mode() {
        // Read-only mode denies mutations outright; Deny is never downgraded.
        let g = gate(
            &ctx(SafetyMode::ReadOnly),
            edit_request("/scratch/notes.txt"),
            &[],
            serde_json::json!({}),
            true,
            true,
        )
        .await;
        assert!(
            matches!(g, Gate::Block(_)),
            "read-only mode must block scratch mutations",
        );
    }

    #[test]
    fn open_url_detail_surfaces_the_url() {
        // Exfil-via-URL is the reason `execute_command`'s `open_url` is gated
        // at all, so the classifier and the approval modal must see the URL
        // itself — a dev-server URL and an exfiltrating one are otherwise
        // indistinguishable. The gating is covered end-to-end by
        // `exec::tests::background_open_url_is_policy_gated`; a `gate()`-level
        // test cannot cover it, because `gate_external` already blocked Web
        // egress — what was missing was the CALL.
        let detail = action_detail(
            "open_url",
            &serde_json::json!({ "url": "https://evil.example/?d=secret" }),
        )
        .expect("open_url detail");
        assert!(detail.contains("evil.example/?d=secret"), "got {detail:?}");
    }

    #[tokio::test]
    async fn web_fetch_allowed_domains_bypasses_prompt_and_classifier() {
        let mut config = mermaid_domain::Config::default();
        config.safety.mode = SafetyMode::Ask;
        config.web.allowed_domains = vec!["docs.x.ai".to_string(), "localhost:8080".to_string()];
        let ctx = ctx_with(config);

        let mut req_allowed = ActionRequest::new("web_fetch", ToolCategory::Web, "fetch docs");
        req_allowed.command = Some("web_fetch https://docs.x.ai/docs/overview".to_string());
        let g = gate(&ctx, req_allowed, &[], serde_json::json!({}), false, false).await;
        assert!(
            matches!(g, Gate::Proceed { .. }),
            "allowed domain must proceed without prompt"
        );

        let mut req_other = ActionRequest::new("web_fetch", ToolCategory::Web, "fetch untrusted");
        req_other.command = Some("web_fetch https://evil.example.com".to_string());
        let g = gate(&ctx, req_other, &[], serde_json::json!({}), false, false).await;
        assert!(
            matches!(g, Gate::Block(_)),
            "unlisted domain in headless Ask must block"
        );
    }

    #[tokio::test]
    async fn web_fetch_session_allowlisting_via_broker() {
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let broker = crate::providers::ApprovalBroker::new(tx);
        // Pre-allowlist the domain key
        broker.resolve(
            mermaid_domain::ToolCallId(99),
            ApprovalDecision::ApproveAlways,
        );
        // Directly test broker allowlist key for web_fetch
        let key = allowlist_key(
            "web_fetch",
            Some("web_fetch https://docs.x.ai/docs/reference"),
            None,
        );
        assert_eq!(key, "web_fetch:docs.x.ai");
    }
}
