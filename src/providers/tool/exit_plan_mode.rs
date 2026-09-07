//! The `exit_plan_mode` tool — the model's only way out of plan mode.
//!
//! Called when the plan file is decision-complete. The tool re-reads the plan
//! from disk (so the user's external edits win over whatever the model last
//! wrote), then parks on the `QuestionBroker` while the TUI shows the
//! approval dialog. Approval returns a `ToolMetadata::Plan` outcome that
//! `handle_tool_finished` uses to clear `session.plan`, seed the checklist
//! from the plan's Tasks section, and (per the user's choice or the `[plan]`
//! config) auto-submit implementation. A request for changes returns the
//! user's feedback and the session stays in plan mode.
//!
//! Registered `is_internal`: `build_chat_request` advertises the definition
//! only while `session.plan` is `Some`, so the model never sees the tool
//! outside plan mode.

use std::time::Instant;

use async_trait::async_trait;

use mermaid_domain::PlanPostApprove;
use mermaid_domain::{
    Question, QuestionKind, QuestionOption, QuestionResolution, ToolDefinition, ToolMetadata,
    ToolOutcome, ToolRunMetadata,
};

use super::super::ctx::ExecContext;
use super::ToolExecutor;

pub struct ExitPlanModeTool;

/// Option labels. Matching is by prefix because the recommended option
/// carries the "(Recommended)" suffix the modal convention expects.
const APPROVE_START: &str = "Approve and start";
const APPROVE_CLEAR: &str = "Approve, clear context and start";
const APPROVE_WAIT: &str = "Approve and wait";
const APPROVE_PINNED: &str = "Approve";
const HAND_OFF: &str = "Hand off...";
const REQUEST_CHANGES: &str = "Request changes";
const HANDOFF_FORK: &str = "Fork of this session";
const HANDOFF_FRESH: &str = "Fresh session";
const SAME_MODEL: &str = "Same model";

/// How execution proceeds after approval, as chosen in the dialog.
#[derive(Clone, Copy, Default)]
struct PlanDecision {
    start: bool,
    fresh: bool,
    fork: bool,
}

fn approved_outcome(
    path: String,
    body: String,
    decision: PlanDecision,
    model: Option<String>,
    secs: f64,
) -> ToolOutcome {
    let text = if decision.fresh || decision.fork {
        // This outcome never reaches a follow-up model call — the reducer
        // swaps conversations instead — but keep it honest for the record.
        "The user approved the plan; execution continues in a new conversation."
    } else if decision.start {
        "The user approved the plan. Plan mode is off and the checklist has been seeded from \
         the plan's Tasks section — implementation starts now."
    } else {
        "The user approved the plan. Plan mode is off and the checklist has been seeded from \
         the plan's Tasks section. Do NOT start implementing until asked; give a one-line \
         acknowledgment and stop."
    };
    ToolOutcome::success(text, "plan approved", secs).with_metadata(ToolRunMetadata {
        detail: ToolMetadata::Plan {
            path,
            body,
            start: decision.start,
            fresh: decision.fresh,
            fork: decision.fork,
            model,
        },
        ..ToolRunMetadata::default()
    })
}

fn changes_outcome(feedback: &str, secs: f64) -> ToolOutcome {
    let text = if feedback.trim().is_empty() {
        "The user requested changes to the plan but left no note. Still in plan mode — ask what \
         they'd like changed."
            .to_string()
    } else {
        format!(
            "The user requested changes to the plan:\n{}\nStill in plan mode — revise the plan \
             file (complete replacement) and call exit_plan_mode again when it is ready.",
            feedback.trim()
        )
    };
    ToolOutcome::success(text, "changes requested", secs)
}

fn option(label: &str, description: &str) -> QuestionOption {
    QuestionOption {
        label: label.to_string(),
        description: Some(description.to_string()),
        recommended: label.to_lowercase().contains("(recommended)"),
        preview: None,
    }
}

impl ExitPlanModeTool {
    /// The two-step handoff dialog: (1) fork vs fresh, (2) execution model.
    /// Both questions ride one modal; a free-typed "Other" on the model
    /// question is any `provider/model` id.
    async fn handoff_flow(
        &self,
        ctx: &ExecContext,
        broker: &crate::providers::QuestionBroker,
        path: String,
        body: String,
        secs: f64,
    ) -> ToolOutcome {
        let mut model_options = vec![option(
            &format!("{SAME_MODEL} (Recommended)"),
            "Keep the current session model.",
        )];
        for candidate in handoff_model_candidates(ctx).await.into_iter().take(12) {
            model_options.push(option(&candidate, "Recently used or local model."));
        }
        let questions = vec![
            Question {
                header: "Hand off".to_string(),
                question: "Continue the approved plan in:".to_string(),
                kind: QuestionKind::Select,
                options: vec![
                    option(
                        &format!("{HANDOFF_FRESH} (Recommended)"),
                        "New conversation seeded with the plan and checklist only.",
                    ),
                    option(
                        HANDOFF_FORK,
                        "New conversation carrying this full transcript.",
                    ),
                ],
                memory_key: None,
            },
            Question {
                header: "Model".to_string(),
                question: "Execute with which model? (type any provider/model as Other)"
                    .to_string(),
                kind: QuestionKind::Select,
                options: model_options,
                memory_key: None,
            },
        ];
        match broker
            .request(&ctx.token, ctx.turn, ctx.call_id, questions)
            .await
        {
            QuestionResolution::Answered { answers, .. } => {
                let pick = |i: usize| {
                    answers
                        .get(i)
                        .map(|a| a.selected.join(", "))
                        .unwrap_or_default()
                };
                let fork = pick(0).starts_with(HANDOFF_FORK);
                let model_choice = pick(1);
                let model = if model_choice.is_empty() || model_choice.starts_with(SAME_MODEL) {
                    None
                } else {
                    Some(model_choice)
                };
                approved_outcome(
                    path,
                    body,
                    PlanDecision {
                        start: true,
                        fresh: !fork,
                        fork,
                    },
                    model,
                    secs,
                )
            },
            QuestionResolution::Dismissed | QuestionResolution::Reformulate => {
                ToolOutcome::success(
                    "The user backed out of the handoff. Still in plan mode — continue the \
                     conversation; call exit_plan_mode again when they are ready.",
                    "handoff cancelled",
                    secs,
                )
            },
        }
    }
}

/// Candidate execution models for the handoff picker: models this install
/// has used before (the per-model reasoning table doubles as a recency
/// record) plus whatever Ollama has installed — read-only, and answered from
/// the on-disk store when the server is stopped, so a sleeping server still
/// contributes candidates. The current model is excluded — "Same model"
/// covers it.
async fn handoff_model_candidates(ctx: &ExecContext) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for m in ctx.config.reasoning_per_model.keys() {
        if *m != ctx.model_id && seen.insert(m.clone()) {
            out.push(m.clone());
        }
    }
    if let Some(models) = crate::ollama::observe_models(&ctx.config).await.models() {
        for name in models {
            let id = format!("ollama/{name}");
            if id != ctx.model_id && seen.insert(id.clone()) {
                out.push(id);
            }
        }
    }
    out
}

#[async_trait]
impl ToolExecutor for ExitPlanModeTool {
    fn name(&self) -> &'static str {
        mermaid_domain::plan::EXIT_PLAN_MODE_TOOL
    }

    fn schema(&self) -> ToolDefinition {
        mermaid_domain::plan::exit_plan_mode_definition()
    }

    /// Excluded from `describe_all`; `build_chat_request` advertises it only
    /// while planning.
    fn is_internal(&self) -> bool {
        true
    }

    #[expect(
        clippy::too_many_lines,
        reason = "the plan approval dialog end to end: read the plan, build the option list for \
         the pinned and unpinned configs, ask, then map each answer label back to its outcome; \
         the answer arms all need the path, body and timer, and they key on the same labels the \
         options were built from, so the two halves belong together"
    )]
    async fn execute(&self, _args: serde_json::Value, ctx: ExecContext) -> ToolOutcome {
        let start_t = Instant::now();
        let secs = || start_t.elapsed().as_secs_f64();

        let Some(plan_path) = ctx.plan_file.clone() else {
            return ToolOutcome::error("exit_plan_mode is only available in plan mode", secs());
        };
        // Fresh read so the user's external edits win over the model's last
        // write — the file on disk IS the plan being approved.
        let body = match tokio::fs::read_to_string(&plan_path).await {
            Ok(b) if !b.trim().is_empty() => b,
            _ => {
                return ToolOutcome::error(
                    format!(
                        "the plan file at {} is missing or empty — write the plan there first, \
                         then call exit_plan_mode again",
                        plan_path.display()
                    ),
                    secs(),
                );
            },
        };
        let display_path = plan_path
            .strip_prefix(&ctx.workdir)
            .unwrap_or(&plan_path)
            .display()
            .to_string();

        let pinned = ctx.config.plan.post_approve;
        let pinned_start = !matches!(pinned, Some(PlanPostApprove::Wait));
        if ctx.config.plan.auto_approve {
            return approved_outcome(
                display_path,
                body,
                PlanDecision {
                    start: pinned_start,
                    ..PlanDecision::default()
                },
                None,
                secs(),
            );
        }
        let Some(broker) = ctx.questions.as_ref() else {
            // Headless: nobody to run the dialog. Approve without starting —
            // the plan is the run's deliverable (the full `--plan` headless
            // flow is a separate feature).
            return approved_outcome(display_path, body, PlanDecision::default(), None, secs());
        };

        let clear_desc = match ctx.context_percent {
            Some(pct) => format!(
                "Fresh conversation seeded with the plan (context: {pct}% used). The \
                 exploration transcript stays on disk."
            ),
            None => "Fresh conversation seeded with the plan. The exploration transcript \
                 stays on disk."
                .to_string(),
        };
        let options = match pinned {
            None => vec![
                option(
                    &format!("{APPROVE_START} (Recommended)"),
                    "End plan mode, seed the checklist, and begin implementing now.",
                ),
                option(APPROVE_CLEAR, &clear_desc),
                option(
                    APPROVE_WAIT,
                    "End plan mode and seed the checklist, but wait at the prompt.",
                ),
                option(
                    HAND_OFF,
                    "Continue in a fork or fresh session, optionally on a different model.",
                ),
                option(
                    REQUEST_CHANGES,
                    "Stay in plan mode; add a note describing what to change.",
                ),
            ],
            Some(_) => vec![
                option(
                    &format!("{APPROVE_PINNED} (Recommended)"),
                    if pinned_start {
                        "End plan mode, seed the checklist, and begin implementing now."
                    } else {
                        "End plan mode and seed the checklist, but wait at the prompt."
                    },
                ),
                option(APPROVE_CLEAR, &clear_desc),
                option(
                    HAND_OFF,
                    "Continue in a fork or fresh session, optionally on a different model.",
                ),
                option(
                    REQUEST_CHANGES,
                    "Stay in plan mode; add a note describing what to change.",
                ),
            ],
        };
        let question = Question {
            header: "Plan".to_string(),
            question: format!("The plan at {display_path} is ready. How should we proceed?"),
            kind: QuestionKind::Select,
            options,
            memory_key: None,
        };

        match broker
            .request(&ctx.token, ctx.turn, ctx.call_id, vec![question])
            .await
        {
            QuestionResolution::Answered { answers, .. } => {
                let answer = answers.first();
                let selected = answer.map(|a| a.selected.join(", ")).unwrap_or_default();
                let note = answer.and_then(|a| a.note.clone()).unwrap_or_default();
                if selected.starts_with(APPROVE_START) {
                    approved_outcome(
                        display_path,
                        body,
                        PlanDecision {
                            start: true,
                            ..PlanDecision::default()
                        },
                        None,
                        secs(),
                    )
                } else if selected.starts_with(APPROVE_CLEAR) {
                    approved_outcome(
                        display_path,
                        body,
                        PlanDecision {
                            start: true,
                            fresh: true,
                            fork: false,
                        },
                        None,
                        secs(),
                    )
                } else if selected.starts_with(HAND_OFF) {
                    return self
                        .handoff_flow(&ctx, broker, display_path, body, secs())
                        .await;
                } else if selected.starts_with(APPROVE_WAIT) {
                    approved_outcome(display_path, body, PlanDecision::default(), None, secs())
                } else if pinned.is_some() && selected.starts_with(APPROVE_PINNED) {
                    approved_outcome(
                        display_path,
                        body,
                        PlanDecision {
                            start: pinned_start,
                            ..PlanDecision::default()
                        },
                        None,
                        secs(),
                    )
                } else if selected.starts_with(REQUEST_CHANGES) {
                    changes_outcome(&note, secs())
                } else {
                    // A typed "Other" answer IS the change request.
                    let feedback = if note.trim().is_empty() {
                        selected
                    } else {
                        format!("{selected}\n{note}")
                    };
                    changes_outcome(&feedback, secs())
                }
            },
            QuestionResolution::Dismissed => ToolOutcome::success(
                "The user dismissed the approval dialog without deciding. Still in plan mode — \
                 do not re-present the plan unprompted; continue the conversation.",
                "approval dismissed",
                secs(),
            ),
            QuestionResolution::Reformulate => ToolOutcome::success(
                "The user chose to discuss rather than decide. Still in plan mode — engage with \
                 what they say next.",
                "user chose to chat first",
                secs(),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changes_outcome_carries_feedback_and_stays_in_plan_mode_wording() {
        let out = changes_outcome("swap PR 3 and 4", 0.0);
        assert!(out.model_content.contains("swap PR 3 and 4"));
        assert!(out.model_content.contains("Still in plan mode"));
        assert!(matches!(out.metadata.detail, ToolMetadata::None));
    }

    #[test]
    fn approved_outcome_rides_the_plan_metadata() {
        let out = approved_outcome(
            ".mermaid/plans/x.md".into(),
            "## Summary".into(),
            PlanDecision {
                start: true,
                ..PlanDecision::default()
            },
            None,
            0.0,
        );
        match &out.metadata.detail {
            ToolMetadata::Plan {
                path,
                body,
                start,
                fresh,
                fork,
                model,
            } => {
                assert_eq!(path, ".mermaid/plans/x.md");
                assert_eq!(body, "## Summary");
                assert!(start);
                assert!(!fresh && !fork);
                assert!(model.is_none());
            },
            other => panic!("expected Plan metadata, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn refuses_outside_plan_mode() {
        use mermaid_domain::{ToolCallId, TurnId};
        let (ctx, _rx) = crate::providers::ctx::test_exec_context(
            TurnId(1),
            ToolCallId(1),
            std::env::temp_dir(),
        );
        // test ctx has plan_file: None.
        let out = ExitPlanModeTool.execute(serde_json::json!({}), ctx).await;
        assert_eq!(out.status, mermaid_domain::ToolStatus::Error);
    }

    #[tokio::test]
    async fn missing_or_empty_plan_file_is_a_teaching_error() {
        use mermaid_domain::{ToolCallId, TurnId};
        let (mut ctx, _rx) = crate::providers::ctx::test_exec_context(
            TurnId(1),
            ToolCallId(1),
            std::env::temp_dir(),
        );
        ctx.plan_file = Some(std::env::temp_dir().join("mermaid_test_plan_missing.md"));
        let out = ExitPlanModeTool.execute(serde_json::json!({}), ctx).await;
        assert_eq!(out.status, mermaid_domain::ToolStatus::Error);
        assert!(out.model_content.contains("write the plan there first"));
    }
}
