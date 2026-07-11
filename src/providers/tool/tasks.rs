//! The task checklist tools: `task_create`, `task_update`, `task_list`.
//!
//! The model's planning surface for multi-step work. Granular and
//! id-addressed (Claude Code's Task-tool shape) with batch arrays in both
//! create and update (codex's one-call ergonomics): the initial plan lands in
//! one `task_create`, and "complete #2 + start #3" is one `task_update`.
//! Every mutation goes through the session's `TaskBroker` (single writer),
//! which publishes a snapshot the TUI renders live.
//!
//! Planning mutates nothing outside the session, so like `ask_user_question`
//! these tools are ungated (they never touch the policy gate) and run in
//! every safety mode, including read_only.
//!
//! Discipline ("exactly one in_progress", no pending->completed jumps) is
//! soft-enforced: violations come back as advisory notes in the tool result,
//! never rejections — a hard reject risks retry loops, while silence (codex)
//! lets malformed checklists render unremarked.

use std::time::Instant;

use async_trait::async_trait;

use crate::domain::tasks::{TaskEdit, TaskItem, TaskOrigin, TaskSpec, TaskStatus, TaskStore};
use crate::domain::{ToolDefinition, ToolMetadata, ToolOutcome, ToolRunMetadata};

use super::super::ctx::ExecContext;
use super::ToolExecutor;

pub struct TaskCreateTool;
pub struct TaskUpdateTool;
pub struct TaskListTool;

/// Tool result line for one task: `#3 [in_progress] wire the broker`.
fn task_line(task: &TaskItem) -> String {
    format!("#{} [{}] {}", task.id, task.status.as_str(), task.subject)
}

/// The checklist as the model sees it from `task_list`: numbered lines plus
/// the progress footer, with per-task evidence indented underneath (the
/// model's own audit trail of what actually happened while each ran).
fn render_list(store: &TaskStore) -> String {
    if store.is_empty() {
        return "No tasks.".to_string();
    }
    let mut out = String::new();
    for task in store.visible() {
        out.push_str(&task_line(task));
        out.push('\n');
        if let Some(desc) = &task.description {
            out.push_str(&format!("    {desc}\n"));
        }
        // Show the tail of the evidence ring — enough to re-anchor after a
        // compaction without ballooning the result.
        for entry in task.evidence.iter().rev().take(3).rev() {
            out.push_str(&format!(
                "    evidence: {} {} ({})\n",
                entry.tool, entry.target, entry.status
            ));
        }
    }
    out.push_str(&store.progress_string());
    out
}

/// Graceful degradation for contexts with no broker (bare test harnesses):
/// the model is told to carry on rather than erroring into a retry loop.
fn no_broker(secs: f64) -> ToolOutcome {
    ToolOutcome::success(
        "Task tracking is unavailable in this context; proceed without it.",
        "tasks unavailable",
        secs,
    )
}

/// Plan mode firewalls the checklist WRITERS: implementation steps belong in
/// the plan file's Tasks section, which seeds the checklist when the plan is
/// approved. Without a hard error models conflate the two planning surfaces
/// (Codex shipped the same runtime error for the same reason). `task_list`
/// stays available — reading is harmless.
fn plan_mode_block(ctx: &crate::providers::ExecContext, secs: f64) -> Option<ToolOutcome> {
    // Only an explicit `allow` in the plan profile unblocks the writers —
    // `auto`/`ask` collapse to deny (ungated tools have no approval path).
    if ctx.plan_permissions.tasks == crate::app::PlanPermLevel::Allow {
        return None;
    }
    ctx.plan_file.as_ref().map(|_| {
        ToolOutcome::error(
            "task tools are disabled in plan mode: the checklist is seeded from the \
             approved plan. Put implementation steps in the plan file's Tasks section \
             instead."
                .to_string(),
            secs,
        )
    })
}

fn metadata(action: &str, store: &TaskStore) -> ToolRunMetadata {
    let (completed, total) = store.counts();
    ToolRunMetadata {
        detail: ToolMetadata::Tasks {
            action: action.to_string(),
            completed: completed as u32,
            total: total as u32,
        },
        ..ToolRunMetadata::default()
    }
}

fn parse_specs(args: &serde_json::Value) -> Result<Vec<TaskSpec>, String> {
    let items = args
        .get("tasks")
        .and_then(|t| t.as_array())
        .ok_or("task_create requires a `tasks` array")?;
    if items.is_empty() {
        return Err("`tasks` must not be empty".to_string());
    }
    items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let subject = item
                .get("subject")
                .and_then(|s| s.as_str())
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| format!("tasks[{i}] is missing `subject`"))?;
            let active_form = item
                .get("active_form")
                .and_then(|s| s.as_str())
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| format!("tasks[{i}] is missing `active_form`"))?;
            let status = item.get("status").and_then(|s| s.as_str());
            let in_progress = match status {
                None | Some("pending") => false,
                Some("in_progress") => true,
                Some(other) => {
                    return Err(format!(
                        "tasks[{i}]: initial status must be \"pending\" or \"in_progress\", got {other:?}"
                    ));
                },
            };
            Ok(TaskSpec {
                subject: subject.to_string(),
                active_form: active_form.to_string(),
                description: item
                    .get("description")
                    .and_then(|s| s.as_str())
                    .map(str::to_string),
                in_progress,
            })
        })
        .collect()
}

fn parse_edits(args: &serde_json::Value) -> Result<Vec<TaskEdit>, String> {
    let items = args
        .get("updates")
        .and_then(|t| t.as_array())
        .ok_or("task_update requires an `updates` array")?;
    if items.is_empty() {
        return Err("`updates` must not be empty".to_string());
    }
    items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let id = item
                .get("id")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| format!("updates[{i}] is missing `id`"))?;
            let status = match item.get("status").and_then(|s| s.as_str()) {
                None => None,
                Some(s) => Some(
                    TaskStatus::parse(s)
                        .ok_or_else(|| format!("updates[{i}]: unknown status {s:?}"))?,
                ),
            };
            Ok(TaskEdit {
                id: id as u32,
                status,
                subject: item
                    .get("subject")
                    .and_then(|s| s.as_str())
                    .map(str::to_string),
                active_form: item
                    .get("active_form")
                    .and_then(|s| s.as_str())
                    .map(str::to_string),
                description: item
                    .get("description")
                    .and_then(|s| s.as_str())
                    .map(str::to_string),
            })
        })
        .collect()
}

#[async_trait]
impl ToolExecutor for TaskCreateTool {
    fn name(&self) -> &'static str {
        "task_create"
    }

    fn schema(&self) -> ToolDefinition {
        ToolDefinition {
            name: "task_create".to_string(),
            description: "Create tasks on your session checklist, which the user sees live in \
                the terminal. Use it at the START of multi-step work (3+ distinct steps): plan \
                the whole job and create ALL initial tasks in ONE call, in execution order. \
                Skip it entirely for trivial or single-step requests — a one-item checklist is \
                noise. Each task needs a short imperative `subject` (\"Wire the broker\") and a \
                present-tense `active_form` (\"Wiring the broker\") shown on the spinner while \
                it runs. Mark at most one task `in_progress`. Add tasks later as you discover \
                work; give an `explanation` when a mid-run addition reshapes the plan."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "tasks": {
                        "type": "array",
                        "description": "Tasks to add, in execution order. Create the full initial plan in one call.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "subject": {
                                    "type": "string",
                                    "description": "Short imperative step, e.g. \"Add the config flag\". Meaningful and verifiable, not vague."
                                },
                                "active_form": {
                                    "type": "string",
                                    "description": "Present-tense form shown while running, e.g. \"Adding the config flag\"."
                                },
                                "description": {
                                    "type": "string",
                                    "description": "Optional detail: acceptance criteria, files involved, constraints."
                                },
                                "status": {
                                    "type": "string",
                                    "enum": ["pending", "in_progress"],
                                    "description": "Initial status (default pending). At most one task in_progress across the whole list."
                                }
                            },
                            "required": ["subject", "active_form"]
                        }
                    },
                    "explanation": {
                        "type": "string",
                        "description": "One-line rationale, when this call reshapes an existing plan. Shown to the user."
                    }
                },
                "required": ["tasks"]
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, ctx: ExecContext) -> ToolOutcome {
        let started = Instant::now();
        let secs = || started.elapsed().as_secs_f64();
        if let Some(blocked) = plan_mode_block(&ctx, secs()) {
            return blocked;
        }
        let Some(broker) = ctx.tasks.clone() else {
            return no_broker(secs());
        };
        let specs = match parse_specs(&args) {
            Ok(s) => s,
            Err(e) => return ToolOutcome::error(e, secs()),
        };
        let count = specs.len();
        let (created, store) = broker.create(specs, TaskOrigin::Model).await;
        let mut out = format!("Created {count} task(s):\n");
        for task in &created {
            out.push_str(&task_line(task));
            out.push('\n');
        }
        out.push_str(&store.progress_string());
        // Creation can violate single-in_progress too (e.g. adding an
        // in_progress task while another is active) — same advisory path.
        for note in crate::domain::advisory_notes(&store, &[], &store) {
            out.push('\n');
            out.push_str(&note);
        }
        ToolOutcome::success(out, format!("created {count} task(s)"), secs())
            .with_metadata(metadata("create", &store))
    }
}

#[async_trait]
impl ToolExecutor for TaskUpdateTool {
    fn name(&self) -> &'static str {
        "task_update"
    }

    fn schema(&self) -> ToolDefinition {
        ToolDefinition {
            name: "task_update".to_string(),
            description: "Update checklist tasks by id (from task_create). Batch related \
                transitions in one call — completing one task and starting the next is ONE \
                call with two updates. Keep exactly one task in_progress at all times: set it \
                in_progress BEFORE you start the work and completed IMMEDIATELY after it is \
                done and verified — never batch-complete at the end, and never jump a task \
                from pending straight to completed. Only mark completed when the work truly \
                succeeded; if you hit a blocker, leave it in_progress and create a task for \
                the blocker. When the plan changes shape (splitting, merging, dropping work), \
                update or delete tasks and say why in `explanation` — do not let the \
                checklist go stale while you work."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "updates": {
                        "type": "array",
                        "description": "Differential updates, applied in order. Only `id` is required; omitted fields stay unchanged.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": {
                                    "type": "integer",
                                    "description": "Task id from task_create."
                                },
                                "status": {
                                    "type": "string",
                                    "enum": ["pending", "in_progress", "completed", "deleted"],
                                    "description": "New status. \"deleted\" permanently removes the task from the list."
                                },
                                "subject": { "type": "string", "description": "Replacement subject." },
                                "active_form": { "type": "string", "description": "Replacement active form." },
                                "description": { "type": "string", "description": "Replacement description." }
                            },
                            "required": ["id"]
                        }
                    },
                    "explanation": {
                        "type": "string",
                        "description": "One-line rationale for scope pivots (deleting, reordering, or reshaping work). Shown to the user."
                    }
                },
                "required": ["updates"]
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, ctx: ExecContext) -> ToolOutcome {
        let started = Instant::now();
        let secs = || started.elapsed().as_secs_f64();
        if let Some(blocked) = plan_mode_block(&ctx, secs()) {
            return blocked;
        }
        let Some(broker) = ctx.tasks.clone() else {
            return no_broker(secs());
        };
        let edits = match parse_edits(&args) {
            Ok(e) => e,
            Err(e) => return ToolOutcome::error(e, secs()),
        };
        let (report, store) = broker.update(edits.clone()).await;
        if report.applied.is_empty() {
            return ToolOutcome::error(
                format!("No updates applied:\n{}", report.errors.join("\n")),
                secs(),
            );
        }
        let mut out = String::new();
        for edit in &edits {
            if !report.applied.contains(&edit.id) {
                continue;
            }
            match edit.status {
                Some(status) => {
                    out.push_str(&format!("#{} -> {}\n", edit.id, status.as_str()));
                },
                None => out.push_str(&format!("#{} updated\n", edit.id)),
            }
        }
        for err in &report.errors {
            out.push_str(&format!("error: {err}\n"));
        }
        out.push_str(&store.progress_string());
        for note in &report.notes {
            out.push('\n');
            out.push_str(note);
        }
        ToolOutcome::success(out, store.progress_string(), secs())
            .with_metadata(metadata("update", &store))
    }
}

#[async_trait]
impl ToolExecutor for TaskListTool {
    fn name(&self) -> &'static str {
        "task_list"
    }

    fn schema(&self) -> ToolDefinition {
        ToolDefinition {
            name: "task_list".to_string(),
            description: "Read back the current session checklist: every task with its id, \
                status, description, and recent evidence (the work recorded while it was in \
                progress). Call it to re-anchor after a context compaction, or when unsure of \
                a task id or the plan's current state. The user also sees this list live in \
                the terminal, so you never need to repeat its contents in prose."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
        }
    }

    async fn execute(&self, _args: serde_json::Value, ctx: ExecContext) -> ToolOutcome {
        let started = Instant::now();
        let secs = || started.elapsed().as_secs_f64();
        let Some(broker) = ctx.tasks.clone() else {
            return no_broker(secs());
        };
        let store = broker.snapshot();
        ToolOutcome::success(render_list(&store), store.progress_string(), secs())
            .with_metadata(metadata("list", &store))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ToolCallId, TurnId};
    use crate::providers::ctx::test_exec_context;
    use crate::providers::tasks::TaskBroker;
    use std::path::PathBuf;

    fn ctx_with_broker() -> (
        ExecContext,
        TaskBroker,
        tokio::sync::mpsc::Receiver<crate::domain::Msg>,
    ) {
        let (mut ctx, _progress) =
            test_exec_context(TurnId(1), ToolCallId(1), PathBuf::from("/tmp"));
        let (tx, rx) = tokio::sync::mpsc::channel(32);
        let broker = TaskBroker::new(tx);
        ctx.tasks = Some(broker.clone());
        (ctx, broker, rx)
    }

    fn create_args(n: usize) -> serde_json::Value {
        let tasks: Vec<serde_json::Value> = (0..n)
            .map(|i| {
                serde_json::json!({
                    "subject": format!("task {i}"),
                    "active_form": format!("doing task {i}"),
                    "status": if i == 0 { "in_progress" } else { "pending" },
                })
            })
            .collect();
        serde_json::json!({ "tasks": tasks })
    }

    #[tokio::test]
    async fn create_returns_ids_and_progress() {
        let (ctx, _broker, _rx) = ctx_with_broker();
        let outcome = TaskCreateTool.execute(create_args(3), ctx).await;
        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert!(outcome.model_content.contains("#1 [in_progress] task 0"));
        assert!(outcome.model_content.contains("#3 [pending] task 2"));
        assert!(outcome.model_content.contains("Tasks 0/3"));
    }

    #[tokio::test]
    async fn update_batches_and_appends_notes() {
        let (ctx, broker, _rx) = ctx_with_broker();
        let outcome = TaskCreateTool.execute(create_args(3), ctx).await;
        assert!(outcome.error.is_none());

        let (ctx2, _p) = test_exec_context(TurnId(1), ToolCallId(2), PathBuf::from("/tmp"));
        let mut ctx2 = ctx2;
        ctx2.tasks = Some(broker.clone());
        let outcome = TaskUpdateTool
            .execute(
                serde_json::json!({ "updates": [
                    { "id": 1, "status": "completed" },
                    { "id": 2, "status": "in_progress" },
                    { "id": 3, "status": "in_progress" },
                ]}),
                ctx2,
            )
            .await;
        assert!(outcome.error.is_none());
        assert!(outcome.model_content.contains("#1 -> completed"));
        assert!(outcome.model_content.contains("Tasks 1/3"));
        // Two in_progress after the batch: the advisory note must land.
        assert!(
            outcome
                .model_content
                .contains("exactly one task in_progress")
        );
        assert_eq!(outcome.summary, "Tasks 1/3");
    }

    #[tokio::test]
    async fn update_all_unknown_ids_is_an_error() {
        let (ctx, _broker, _rx) = ctx_with_broker();
        let outcome = TaskUpdateTool
            .execute(
                serde_json::json!({ "updates": [{ "id": 42, "status": "completed" }]}),
                ctx,
            )
            .await;
        assert!(outcome.error.is_some());
        assert!(
            outcome
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("no such task")
        );
    }

    #[tokio::test]
    async fn list_renders_descriptions_and_evidence() {
        let (ctx, broker, _rx) = ctx_with_broker();
        let outcome = TaskCreateTool
            .execute(
                serde_json::json!({ "tasks": [{
                    "subject": "wire broker",
                    "active_form": "wiring broker",
                    "description": "through ExecContext",
                    "status": "in_progress",
                }]}),
                ctx,
            )
            .await;
        assert!(outcome.error.is_none());
        broker
            .record_evidence(crate::domain::EvidenceEntry {
                tool: "edit_file".into(),
                target: "src/x.rs".into(),
                status: "ok".into(),
            })
            .await;

        let (mut ctx2, _p) = test_exec_context(TurnId(1), ToolCallId(3), PathBuf::from("/tmp"));
        ctx2.tasks = Some(broker);
        let outcome = TaskListTool.execute(serde_json::json!({}), ctx2).await;
        assert!(
            outcome
                .model_content
                .contains("#1 [in_progress] wire broker")
        );
        assert!(outcome.model_content.contains("    through ExecContext"));
        assert!(
            outcome
                .model_content
                .contains("evidence: edit_file src/x.rs (ok)")
        );
    }

    #[tokio::test]
    async fn missing_broker_degrades_gracefully() {
        let (ctx, _p) = test_exec_context(TurnId(1), ToolCallId(1), PathBuf::from("/tmp"));
        let outcome = TaskCreateTool.execute(create_args(1), ctx).await;
        assert!(outcome.error.is_none());
        assert!(outcome.model_content.contains("unavailable"));
    }

    #[tokio::test]
    async fn create_rejects_malformed_args() {
        let (ctx, _broker, _rx) = ctx_with_broker();
        let outcome = TaskCreateTool
            .execute(serde_json::json!({ "tasks": [] }), ctx)
            .await;
        assert!(outcome.error.is_some());

        let (mut ctx2, _p) = test_exec_context(TurnId(1), ToolCallId(2), PathBuf::from("/tmp"));
        let (tx, _rx2) = tokio::sync::mpsc::channel(8);
        ctx2.tasks = Some(TaskBroker::new(tx));
        let outcome = TaskCreateTool
            .execute(serde_json::json!({ "tasks": [{ "subject": "x" }] }), ctx2)
            .await;
        assert!(
            outcome
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("active_form")
        );
    }
}
