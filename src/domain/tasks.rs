//! The task checklist: differential, id-addressed todo tracking.
//!
//! Pure data + pure operations. The `TaskBroker` (`crate::providers::tasks`)
//! owns the authoritative store and is the only writer; every mutation
//! publishes a full snapshot via `Msg::TasksUpdated`, and the reducer's copy
//! on `Session.tasks` is render/persist truth. Timestamps and token readings
//! arrive here as plain arguments ([`Stamp`]) — nothing in this module reads
//! a clock, so it stays inside the domain-purity fence and `--replay` stays
//! deterministic.
//!
//! Design synthesizes Claude Code's granular Task tools (stable ids,
//! differential updates) with codex's `update_plan` (batch calls, per-update
//! `explanation`), plus mermaid-only extensions: per-task cost stamps, a
//! bounded evidence ring, and user-originated tasks (`/tasks add`).

use serde::{Deserialize, Serialize};

/// Lifecycle of one checklist item. `Deleted` items stay in the store as
/// tombstones so ids are never reused or re-resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
    Deleted,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Deleted => "deleted",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "in_progress" => Some(Self::InProgress),
            "completed" => Some(Self::Completed),
            "deleted" => Some(Self::Deleted),
            _ => None,
        }
    }
}

/// Who created the task. User-originated tasks (`/tasks add`) render with a
/// marker and are called out to the model in a turn-start notice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskOrigin {
    #[default]
    Model,
    User,
}

/// One recorded piece of work attributed to a task while it was in progress:
/// which tool ran, against what, and how it ended.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvidenceEntry {
    pub tool: String,
    pub target: String,
    pub status: String,
}

/// Cap on the per-task evidence ring; oldest entries are dropped first.
pub const EVIDENCE_CAP: usize = 20;

/// Wall-clock + run-token reading supplied by the impure side at mutation
/// time. Optional everywhere so pure tests and replay never fabricate one.
#[derive(Debug, Clone, Copy, Default)]
pub struct Stamp {
    /// Seconds since the Unix epoch.
    pub now_epoch: u64,
    /// The run's committed-token counter at this moment.
    pub run_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskItem {
    /// Stable, monotonic, never reused.
    pub id: u32,
    /// Short imperative description ("Wire the broker into ExecContext").
    pub subject: String,
    /// Present-tense form shown on the spinner line while in progress.
    pub active_form: String,
    #[serde(default)]
    pub description: Option<String>,
    pub status: TaskStatus,
    #[serde(default)]
    pub origin: TaskOrigin,
    /// Epoch seconds when the task first entered `InProgress`.
    #[serde(default)]
    pub started_at: Option<u64>,
    /// Epoch seconds when the task entered `Completed`.
    #[serde(default)]
    pub completed_at: Option<u64>,
    /// Run-token counter reading when the task entered `InProgress`;
    /// bookkeeping for `tokens_spent`, not user-facing.
    #[serde(default)]
    pub tokens_at_start: Option<u64>,
    /// Run-token delta between entering `InProgress` and `Completed`.
    #[serde(default)]
    pub tokens_spent: Option<u64>,
    /// Bounded ring of work performed while this task was in progress.
    #[serde(default)]
    pub evidence: Vec<EvidenceEntry>,
}

impl TaskItem {
    /// Elapsed seconds from start to completion, when both stamps exist.
    pub fn elapsed_secs(&self) -> Option<u64> {
        match (self.started_at, self.completed_at) {
            (Some(s), Some(c)) => Some(c.saturating_sub(s)),
            (Some(_), None) | (None, Some(_)) | (None, None) => None,
        }
    }
}

/// A new task requested via `task_create` (or `/tasks add`), before an id is
/// assigned.
#[derive(Debug, Clone)]
pub struct TaskSpec {
    pub subject: String,
    pub active_form: String,
    pub description: Option<String>,
    pub in_progress: bool,
}

/// One differential edit requested via `task_update` (or `/tasks rm|done`).
/// Only `id` is required; absent fields are left untouched.
#[derive(Debug, Clone, Default)]
pub struct TaskEdit {
    pub id: u32,
    pub status: Option<TaskStatus>,
    pub subject: Option<String>,
    pub active_form: Option<String>,
    pub description: Option<String>,
}

/// A checklist edit made by the user via `/tasks` (not the model). Carried
/// on `Cmd::UserTaskEdit` to the effect runner, which applies it through the
/// `TaskBroker` — the single writer — so user edits and concurrent tool calls
/// serialize instead of racing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserTaskEdit {
    Add { subject: String },
    Remove { id: u32 },
    Done { id: u32 },
    Clear,
}

/// Result of applying a batch of [`TaskEdit`]s: which ids were applied,
/// per-item errors (unknown/deleted ids), and soft-validation notes for the
/// model. Errors never abort the rest of the batch.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ApplyReport {
    pub applied: Vec<u32>,
    pub errors: Vec<String>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TaskStore {
    #[serde(default)]
    pub tasks: Vec<TaskItem>,
    /// Last id handed out; 0 means the first task gets id 1.
    #[serde(default)]
    pub next_id: u32,
}

impl TaskStore {
    /// Append new tasks in order, assigning ids. Returns the assigned ids.
    /// A spec flagged `in_progress` is stamped as started.
    pub fn create(&mut self, specs: Vec<TaskSpec>, origin: TaskOrigin, stamp: Stamp) -> Vec<u32> {
        let mut ids = Vec::with_capacity(specs.len());
        for spec in specs {
            self.next_id += 1;
            let in_progress = spec.in_progress;
            self.tasks.push(TaskItem {
                id: self.next_id,
                subject: spec.subject,
                active_form: spec.active_form,
                description: spec.description,
                status: if in_progress {
                    TaskStatus::InProgress
                } else {
                    TaskStatus::Pending
                },
                origin,
                started_at: in_progress.then_some(stamp.now_epoch),
                completed_at: None,
                tokens_at_start: in_progress.then_some(stamp.run_tokens),
                tokens_spent: None,
                evidence: Vec::new(),
            });
            ids.push(self.next_id);
        }
        ids
    }

    /// Apply each edit independently; unknown or deleted ids become per-item
    /// errors, valid edits still land. Status transitions pick up cost stamps.
    /// Soft-validation notes compare the store before and after the batch.
    pub fn apply(&mut self, edits: &[TaskEdit], stamp: Stamp) -> ApplyReport {
        let before = self.clone();
        let mut report = ApplyReport::default();
        for edit in edits {
            let Some(task) = self.tasks.iter_mut().find(|t| t.id == edit.id) else {
                report.errors.push(format!("#{}: no such task", edit.id));
                continue;
            };
            if task.status == TaskStatus::Deleted && edit.status != Some(TaskStatus::Deleted) {
                report
                    .errors
                    .push(format!("#{}: task was deleted; create a new one", edit.id));
                continue;
            }
            if let Some(subject) = &edit.subject {
                task.subject = subject.clone();
            }
            if let Some(active_form) = &edit.active_form {
                task.active_form = active_form.clone();
            }
            if let Some(description) = &edit.description {
                task.description = Some(description.clone());
            }
            if let Some(status) = edit.status {
                transition(task, status, stamp);
            }
            report.applied.push(edit.id);
        }
        report.notes = advisory_notes(&before, edits, self);
        report
    }

    /// Record evidence against the current in-progress task, if any.
    /// Returns whether an entry was recorded.
    pub fn record_evidence(&mut self, entry: EvidenceEntry) -> bool {
        let Some(task) = self
            .tasks
            .iter_mut()
            .find(|t| t.status == TaskStatus::InProgress)
        else {
            return false;
        };
        if task.evidence.len() >= EVIDENCE_CAP {
            task.evidence.remove(0);
        }
        task.evidence.push(entry);
        true
    }

    /// All non-deleted tasks, in creation order.
    pub fn visible(&self) -> impl Iterator<Item = &TaskItem> {
        self.tasks
            .iter()
            .filter(|t| t.status != TaskStatus::Deleted)
    }

    /// `(completed, total)` over visible tasks.
    pub fn counts(&self) -> (usize, usize) {
        let mut completed = 0;
        let mut total = 0;
        for task in self.visible() {
            total += 1;
            if task.status == TaskStatus::Completed {
                completed += 1;
            }
        }
        (completed, total)
    }

    /// Compact progress label, e.g. `Tasks 2/5`.
    pub fn progress_string(&self) -> String {
        let (completed, total) = self.counts();
        format!("Tasks {completed}/{total}")
    }

    pub fn is_empty(&self) -> bool {
        self.visible().next().is_none()
    }

    pub fn all_done(&self) -> bool {
        let (completed, total) = self.counts();
        total > 0 && completed == total
    }

    /// The current in-progress task (first, if the model broke discipline).
    pub fn active(&self) -> Option<&TaskItem> {
        self.tasks
            .iter()
            .find(|t| t.status == TaskStatus::InProgress)
    }

    /// The next pending task, in creation order.
    pub fn next_pending(&self) -> Option<&TaskItem> {
        self.tasks.iter().find(|t| t.status == TaskStatus::Pending)
    }

    /// Tasks that flipped to `Completed` relative to `before` — drives the
    /// `task_completed` hook.
    pub fn newly_completed<'a>(&'a self, before: &TaskStore) -> Vec<&'a TaskItem> {
        self.visible()
            .filter(|t| {
                t.status == TaskStatus::Completed
                    && before
                        .tasks
                        .iter()
                        .find(|b| b.id == t.id)
                        .is_none_or(|b| b.status != TaskStatus::Completed)
            })
            .collect()
    }
}

/// Apply a status change, maintaining cost stamps on the edges:
/// entering `InProgress` stamps start time/tokens (first entry only —
/// a veto re-entry keeps the original start), entering `Completed`
/// stamps completion and the token delta.
fn transition(task: &mut TaskItem, status: TaskStatus, stamp: Stamp) {
    if task.status == status {
        return;
    }
    match status {
        TaskStatus::InProgress => {
            if task.started_at.is_none() {
                task.started_at = Some(stamp.now_epoch);
                task.tokens_at_start = Some(stamp.run_tokens);
            }
            // Re-opened after completion or a vetoed completion: clear the end stamps.
            task.completed_at = None;
            task.tokens_spent = None;
        },
        TaskStatus::Completed => {
            task.completed_at = Some(stamp.now_epoch);
            task.tokens_spent = task
                .tokens_at_start
                .map(|start| stamp.run_tokens.saturating_sub(start));
        },
        TaskStatus::Pending | TaskStatus::Deleted => {},
    }
    task.status = status;
}

/// Soft validation: advisory notes appended to the tool result, never a
/// rejection (a hard reject risks retry loops; codex's enforce-nothing
/// approach lets malformed checklists render silently). Strictness changes
/// edit this list of checks only.
pub fn advisory_notes(before: &TaskStore, edits: &[TaskEdit], after: &TaskStore) -> Vec<String> {
    let mut notes = Vec::new();
    notes.extend(check_single_in_progress(after));
    notes.extend(check_no_status_jump(before, edits));
    notes
}

fn check_single_in_progress(after: &TaskStore) -> Option<String> {
    let in_progress: Vec<u32> = after
        .visible()
        .filter(|t| t.status == TaskStatus::InProgress)
        .map(|t| t.id)
        .collect();
    (in_progress.len() > 1).then(|| {
        let ids = in_progress
            .iter()
            .map(|id| format!("#{id}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!("Note: {ids} are all in_progress; keep exactly one task in_progress at a time.")
    })
}

fn check_no_status_jump(before: &TaskStore, edits: &[TaskEdit]) -> Option<String> {
    let jumped: Vec<u32> = edits
        .iter()
        .filter(|e| e.status == Some(TaskStatus::Completed))
        .filter(|e| {
            before
                .tasks
                .iter()
                .any(|t| t.id == e.id && t.status == TaskStatus::Pending)
        })
        .map(|e| e.id)
        .collect();
    (!jumped.is_empty()).then(|| {
        let ids = jumped
            .iter()
            .map(|id| format!("#{id}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "Note: {ids} jumped from pending straight to completed; mark a task in_progress while working on it."
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(subject: &str) -> TaskSpec {
        TaskSpec {
            subject: subject.into(),
            active_form: format!("{subject}ing"),
            description: None,
            in_progress: false,
        }
    }

    fn store_with(n: u32) -> TaskStore {
        let mut store = TaskStore::default();
        store.create(
            (0..n).map(|i| spec(&format!("task {i}"))).collect(),
            TaskOrigin::Model,
            Stamp::default(),
        );
        store
    }

    fn edit(id: u32, status: TaskStatus) -> TaskEdit {
        TaskEdit {
            id,
            status: Some(status),
            ..TaskEdit::default()
        }
    }

    #[test]
    fn ids_are_monotonic_across_deletes() {
        let mut store = store_with(2);
        store.apply(&[edit(2, TaskStatus::Deleted)], Stamp::default());
        let ids = store.create(vec![spec("later")], TaskOrigin::Model, Stamp::default());
        assert_eq!(ids, vec![3]);
        assert_eq!(store.visible().count(), 2);
    }

    #[test]
    fn apply_reports_partial_failure() {
        let mut store = store_with(1);
        let report = store.apply(
            &[
                edit(1, TaskStatus::InProgress),
                edit(9, TaskStatus::Completed),
            ],
            Stamp::default(),
        );
        assert_eq!(report.applied, vec![1]);
        assert_eq!(report.errors, vec!["#9: no such task"]);
    }

    #[test]
    fn deleted_tasks_reject_edits_and_hide() {
        let mut store = store_with(1);
        store.apply(&[edit(1, TaskStatus::Deleted)], Stamp::default());
        let report = store.apply(&[edit(1, TaskStatus::InProgress)], Stamp::default());
        assert!(report.applied.is_empty());
        assert_eq!(report.errors.len(), 1);
        assert!(store.is_empty());
    }

    #[test]
    fn dual_in_progress_yields_note() {
        let mut store = store_with(2);
        let report = store.apply(
            &[
                edit(1, TaskStatus::InProgress),
                edit(2, TaskStatus::InProgress),
            ],
            Stamp::default(),
        );
        assert_eq!(report.notes.len(), 1);
        assert!(report.notes[0].contains("#1, #2"));
    }

    #[test]
    fn pending_to_completed_jump_yields_note() {
        let mut store = store_with(1);
        let report = store.apply(&[edit(1, TaskStatus::Completed)], Stamp::default());
        assert_eq!(report.notes.len(), 1);
        assert!(report.notes[0].contains("pending straight to completed"));
    }

    #[test]
    fn clean_update_yields_no_notes() {
        let mut store = store_with(2);
        store.apply(&[edit(1, TaskStatus::InProgress)], Stamp::default());
        let report = store.apply(
            &[
                edit(1, TaskStatus::Completed),
                edit(2, TaskStatus::InProgress),
            ],
            Stamp::default(),
        );
        assert!(report.notes.is_empty(), "{:?}", report.notes);
        assert!(report.errors.is_empty());
    }

    #[test]
    fn cost_stamps_ride_the_transitions() {
        let mut store = store_with(1);
        store.apply(
            &[edit(1, TaskStatus::InProgress)],
            Stamp {
                now_epoch: 100,
                run_tokens: 1_000,
            },
        );
        store.apply(
            &[edit(1, TaskStatus::Completed)],
            Stamp {
                now_epoch: 230,
                run_tokens: 9_400,
            },
        );
        let task = &store.tasks[0];
        assert_eq!(task.elapsed_secs(), Some(130));
        assert_eq!(task.tokens_spent, Some(8_400));
    }

    #[test]
    fn veto_reopen_keeps_original_start() {
        let mut store = store_with(1);
        store.apply(
            &[edit(1, TaskStatus::InProgress)],
            Stamp {
                now_epoch: 100,
                run_tokens: 10,
            },
        );
        store.apply(
            &[edit(1, TaskStatus::Completed)],
            Stamp {
                now_epoch: 200,
                run_tokens: 20,
            },
        );
        store.apply(
            &[edit(1, TaskStatus::InProgress)],
            Stamp {
                now_epoch: 300,
                run_tokens: 30,
            },
        );
        let task = &store.tasks[0];
        assert_eq!(task.started_at, Some(100));
        assert_eq!(task.completed_at, None);
        assert_eq!(task.tokens_spent, None);
    }

    #[test]
    fn evidence_ring_is_bounded_and_targets_active() {
        let mut store = store_with(2);
        assert!(!store.record_evidence(EvidenceEntry {
            tool: "edit_file".into(),
            target: "a.rs".into(),
            status: "ok".into(),
        }));
        store.apply(&[edit(2, TaskStatus::InProgress)], Stamp::default());
        for i in 0..(EVIDENCE_CAP + 5) {
            assert!(store.record_evidence(EvidenceEntry {
                tool: "execute_command".into(),
                target: format!("cmd {i}"),
                status: "ok".into(),
            }));
        }
        let task = store.tasks.iter().find(|t| t.id == 2).unwrap();
        assert_eq!(task.evidence.len(), EVIDENCE_CAP);
        assert_eq!(task.evidence[0].target, "cmd 5");
    }

    #[test]
    fn progress_and_active_and_next() {
        let mut store = store_with(3);
        store.apply(&[edit(1, TaskStatus::InProgress)], Stamp::default());
        store.apply(
            &[
                edit(1, TaskStatus::Completed),
                edit(2, TaskStatus::InProgress),
            ],
            Stamp::default(),
        );
        assert_eq!(store.progress_string(), "Tasks 1/3");
        assert_eq!(store.active().unwrap().id, 2);
        assert_eq!(store.next_pending().unwrap().id, 3);
        assert!(!store.all_done());
    }

    #[test]
    fn newly_completed_diff() {
        let mut store = store_with(2);
        store.apply(&[edit(1, TaskStatus::InProgress)], Stamp::default());
        let before = store.clone();
        store.apply(&[edit(1, TaskStatus::Completed)], Stamp::default());
        let fresh = store.newly_completed(&before);
        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh[0].id, 1);
        // Re-diff against the new state: nothing fresh.
        assert!(store.newly_completed(&store.clone()).is_empty());
    }

    #[test]
    fn serde_roundtrip_and_legacy_defaults() {
        let mut store = store_with(1);
        store.apply(&[edit(1, TaskStatus::InProgress)], Stamp::default());
        let json = serde_json::to_string(&store).unwrap();
        let back: TaskStore = serde_json::from_str(&json).unwrap();
        assert_eq!(store, back);
        // A minimal item (as an older snapshot would carry) still loads.
        let legacy: TaskItem =
            serde_json::from_str(r#"{"id":1,"subject":"s","active_form":"a","status":"pending"}"#)
                .unwrap();
        assert_eq!(legacy.origin, TaskOrigin::Model);
        assert!(legacy.evidence.is_empty());
    }
}
