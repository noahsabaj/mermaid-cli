//! Single-writer broker for the task checklist.
//!
//! Owns the authoritative [`TaskStore`]. The three task tools, `/tasks` user
//! edits (`Cmd::UserTaskEdit`), evidence recording, and the `task_completed`
//! veto path all mutate through here; every mutation publishes a full
//! snapshot as `Msg::TasksUpdated`, which the reducer copies onto
//! `conversation.tasks` for render + persistence. Serializing every writer
//! through one lock is what lets a `/tasks add` land safely while a turn's
//! tool call is mid-flight.
//!
//! Unlike `QuestionBroker` there is no parking: every operation is
//! fire-and-forget, tools never block on the user.
//!
//! Lock discipline matches the other brokers: [`std::sync::Mutex`] (guard is
//! `!Send`, so holding it across an `.await` fails to compile); mutate, clone
//! the snapshot, drop the guard, then publish.
//!
//! Cost stamps: the broker reads the wall clock (impure side — fine) and the
//! latest token reading pushed by the effect runner via [`note_tokens`].
//! Stamps flow into the domain as plain data on the snapshot, so `--replay`
//! reproduces them from the recorded `Msg` instead of recomputing.
//!
//! [`note_tokens`]: TaskBroker::note_tokens

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use crate::domain::Msg;
use crate::domain::tasks::{
    ApplyReport, EvidenceEntry, Stamp, TaskEdit, TaskItem, TaskOrigin, TaskSpec, TaskStatus,
    TaskStore, UserTaskEdit,
};

#[derive(Clone)]
pub struct TaskBroker {
    store: Arc<Mutex<TaskStore>>,
    /// Session-monotonic completion-token counter, accumulated by the effect
    /// runner as providers report usage. Task cost deltas are computed
    /// between the readings at in_progress and completed.
    tokens: Arc<AtomicU64>,
    msg_tx: mpsc::Sender<Msg>,
}

impl TaskBroker {
    pub fn new(msg_tx: mpsc::Sender<Msg>) -> Self {
        Self {
            store: Arc::new(Mutex::new(TaskStore::default())),
            tokens: Arc::new(AtomicU64::new(0)),
            msg_tx,
        }
    }

    /// Overwrite the store wholesale: startup resume seeding, rewind/fork
    /// and `/clear` (empty store). No publish — the reducer already holds
    /// this truth; it is telling us, not the other way around.
    pub fn seed(&self, store: TaskStore) {
        *self.lock() = store;
    }

    /// Accumulate a completed request's completion tokens into the
    /// session-monotonic counter. Called by the effect runner on every
    /// provider usage report; task cost deltas read this counter at the
    /// in_progress and completed edges.
    pub fn add_tokens(&self, completion_tokens: u64) {
        self.tokens.fetch_add(completion_tokens, Ordering::Relaxed);
    }

    /// Append new tasks; returns the created items and publishes.
    pub async fn create(
        &self,
        specs: Vec<TaskSpec>,
        origin: TaskOrigin,
    ) -> (Vec<TaskItem>, TaskStore) {
        let (created, snapshot) = {
            let mut store = self.lock();
            let ids = store.create(specs, origin, self.stamp());
            let created = store
                .tasks
                .iter()
                .filter(|t| ids.contains(&t.id))
                .cloned()
                .collect();
            (created, store.clone())
        };
        self.publish(snapshot.clone()).await;
        (created, snapshot)
    }

    /// Apply differential edits; returns the per-item report (with advisory
    /// notes) and publishes.
    pub async fn update(&self, edits: Vec<TaskEdit>) -> (ApplyReport, TaskStore) {
        let (report, snapshot) = {
            let mut store = self.lock();
            let report = store.apply(&edits, self.stamp());
            (report, store.clone())
        };
        self.publish(snapshot.clone()).await;
        (report, snapshot)
    }

    /// Apply a `/tasks` user edit. Returns the outcome line shown in the
    /// transcript and the id it affected (for the model notice).
    pub async fn user_edit(&self, edit: UserTaskEdit) -> (String, TaskStore) {
        let (line, snapshot) = {
            let mut store = self.lock();
            let subject_of = |store: &TaskStore, id: u32| {
                store
                    .tasks
                    .iter()
                    .find(|t| t.id == id)
                    .map(|t| t.subject.clone())
                    .unwrap_or_default()
            };
            let line = match edit {
                UserTaskEdit::Add { subject } => {
                    let ids = store.create(
                        vec![TaskSpec {
                            active_form: subject.clone(),
                            subject: subject.clone(),
                            description: None,
                            in_progress: false,
                        }],
                        TaskOrigin::User,
                        self.stamp(),
                    );
                    format!("Added task #{} '{subject}'", ids[0])
                },
                UserTaskEdit::Remove { id } => {
                    let subject = subject_of(&store, id);
                    let report = store.apply(
                        &[TaskEdit {
                            id,
                            status: Some(TaskStatus::Deleted),
                            ..TaskEdit::default()
                        }],
                        self.stamp(),
                    );
                    match report.errors.first() {
                        Some(err) => err.clone(),
                        None => format!("Removed task #{id} '{subject}'"),
                    }
                },
                UserTaskEdit::Done { id } => {
                    let subject = subject_of(&store, id);
                    let report = store.apply(
                        &[TaskEdit {
                            id,
                            status: Some(TaskStatus::Completed),
                            ..TaskEdit::default()
                        }],
                        self.stamp(),
                    );
                    match report.errors.first() {
                        Some(err) => err.clone(),
                        None => format!("Marked task #{id} '{subject}' completed"),
                    }
                },
                UserTaskEdit::Clear => {
                    *store = TaskStore::default();
                    "Cleared the task list".to_string()
                },
            };
            (line, store.clone())
        };
        self.publish(snapshot.clone()).await;
        (line, snapshot)
    }

    /// Attach evidence to the current in-progress task, if any. Publishes
    /// only when something was recorded.
    pub async fn record_evidence(&self, entry: EvidenceEntry) {
        let snapshot = {
            let mut store = self.lock();
            store.record_evidence(entry).then(|| store.clone())
        };
        if let Some(snapshot) = snapshot {
            self.publish(snapshot).await;
        }
    }

    pub fn snapshot(&self) -> TaskStore {
        self.lock().clone()
    }

    fn stamp(&self) -> Stamp {
        Stamp {
            now_epoch: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            run_tokens: self.tokens.load(Ordering::Relaxed),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, TaskStore> {
        self.store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Fire-and-forget snapshot to the reducer. A closed channel (shutdown)
    /// is ignored — the broker's copy is still truth for any later reader.
    async fn publish(&self, store: TaskStore) {
        let _ = self.msg_tx.send(Msg::TasksUpdated { store }).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(subject: &str, in_progress: bool) -> TaskSpec {
        TaskSpec {
            subject: subject.into(),
            active_form: format!("{subject}ing"),
            description: None,
            in_progress,
        }
    }

    async fn recv_store(rx: &mut mpsc::Receiver<Msg>) -> TaskStore {
        match rx.recv().await {
            Some(Msg::TasksUpdated { store }) => store,
            other => panic!("expected TasksUpdated, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn create_and_update_publish_snapshots() {
        let (tx, mut rx) = mpsc::channel(8);
        let broker = TaskBroker::new(tx);
        let (created, _) = broker
            .create(vec![spec("a", true), spec("b", false)], TaskOrigin::Model)
            .await;
        assert_eq!(created.len(), 2);
        assert_eq!(recv_store(&mut rx).await.counts(), (0, 2));

        let (report, _) = broker
            .update(vec![TaskEdit {
                id: created[0].id,
                status: Some(TaskStatus::Completed),
                ..TaskEdit::default()
            }])
            .await;
        assert!(report.errors.is_empty());
        let published = recv_store(&mut rx).await;
        assert_eq!(published.counts(), (1, 2));
    }

    #[tokio::test]
    async fn token_readings_feed_cost_stamps() {
        let (tx, _rx) = mpsc::channel(8);
        let broker = TaskBroker::new(tx);
        broker.add_tokens(1_000);
        let (created, _) = broker
            .create(vec![spec("a", true)], TaskOrigin::Model)
            .await;
        broker.add_tokens(8_400);
        let (_, snapshot) = broker
            .update(vec![TaskEdit {
                id: created[0].id,
                status: Some(TaskStatus::Completed),
                ..TaskEdit::default()
            }])
            .await;
        assert_eq!(snapshot.tasks[0].tokens_spent, Some(8_400));
    }

    #[tokio::test]
    async fn seed_overwrites_without_publishing() {
        let (tx, mut rx) = mpsc::channel(8);
        let broker = TaskBroker::new(tx);
        let mut store = TaskStore::default();
        store.create(
            vec![spec("seeded", false)],
            TaskOrigin::Model,
            Stamp::default(),
        );
        broker.seed(store);
        assert_eq!(broker.snapshot().tasks.len(), 1);
        assert!(rx.try_recv().is_err(), "seed must not publish");
    }

    #[tokio::test]
    async fn user_edits_apply_and_report() {
        let (tx, mut rx) = mpsc::channel(8);
        let broker = TaskBroker::new(tx);
        let (line, _) = broker
            .user_edit(UserTaskEdit::Add {
                subject: "review the docs".into(),
            })
            .await;
        assert_eq!(line, "Added task #1 'review the docs'");
        assert_eq!(recv_store(&mut rx).await.tasks[0].origin, TaskOrigin::User);

        let (line, snapshot) = broker.user_edit(UserTaskEdit::Remove { id: 9 }).await;
        assert_eq!(line, "#9: no such task");
        assert_eq!(snapshot.visible().count(), 1);
    }

    #[tokio::test]
    async fn evidence_publishes_only_when_recorded() {
        let (tx, mut rx) = mpsc::channel(8);
        let broker = TaskBroker::new(tx);
        // No in-progress task yet: nothing recorded, nothing published.
        broker
            .record_evidence(EvidenceEntry {
                tool: "edit_file".into(),
                target: "a.rs".into(),
                status: "ok".into(),
            })
            .await;
        assert!(rx.try_recv().is_err());

        broker
            .create(vec![spec("a", true)], TaskOrigin::Model)
            .await;
        let _ = recv_store(&mut rx).await;
        broker
            .record_evidence(EvidenceEntry {
                tool: "edit_file".into(),
                target: "a.rs".into(),
                status: "ok".into(),
            })
            .await;
        let published = recv_store(&mut rx).await;
        assert_eq!(published.tasks[0].evidence.len(), 1);
    }
}
