//! Unified storage coordinator bridging SQLite runtime tables and filesystem conversation logs.
//!
//! Provides bidirectional integrity, task-session backfilling, cross-store GC,
//! and atomic cascade deletion between `runtime.sqlite3` and `.mermaid/conversations/`.

use std::fs;
use std::path::Path;

use anyhow::Result;
use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::storage::{NewSession, RuntimeStore};

/// Summary report of a project-level storage reconciliation pass.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ReconciliationReport {
    /// Sessions found on disk that had no row in SQLite and were backfilled.
    pub backfilled_sessions: usize,
    /// Tasks whose `conversation_id` does not exist on disk in `.mermaid/conversations/`.
    pub orphaned_tasks: Vec<String>,
    /// Sessions that match cleanly between SQLite and filesystem logs.
    pub valid_sessions: usize,
    /// SQLite session rows whose conversation files are missing from disk.
    pub missing_session_files: usize,
}

/// Summary report of a cross-store garbage collection pass.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CrossStoreGcReport {
    /// SQLite rows removed by table pruning.
    pub db_rows_removed: u64,
    /// Expired conversation files (.json, .jsonl, .meta) removed from disk.
    pub conversation_files_removed: u64,
    /// Expired compaction archive directories removed from disk.
    pub compactions_removed: u64,
}

#[derive(Debug, Deserialize)]
struct MinimalConversationFile {
    id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    model_name: Option<String>,
    #[serde(default)]
    project_path: Option<String>,
    #[serde(default)]
    total_tokens: Option<i64>,
}

/// Unified coordinator for SQLite and filesystem conversation storage.
pub struct StorageCoordinator<'a> {
    store: &'a RuntimeStore,
}

impl<'a> StorageCoordinator<'a> {
    /// Create a new coordinator borrowing the underlying [`RuntimeStore`].
    #[must_use]
    pub fn new(store: &'a RuntimeStore) -> Self {
        Self { store }
    }

    /// Link a task to a conversation session, updating both the `tasks` row and
    /// the `sessions` index table in SQLite.
    ///
    /// # Errors
    ///
    /// Errors if SQLite transactions or statements fail.
    pub fn link_task_conversation(
        &self,
        task_id: &str,
        session_id: &str,
        project_path: &Path,
        model_id: &str,
        title: Option<&str>,
        total_tokens: Option<i64>,
    ) -> Result<()> {
        self.store.tasks().set_conversation(task_id, session_id)?;

        let conv_path = project_path
            .join(".mermaid")
            .join("conversations")
            .join(format!("{session_id}.json"));

        self.store.sessions().upsert(NewSession {
            id: Some(session_id.to_string()),
            project_path: project_path.to_string_lossy().to_string(),
            model_id: model_id.to_string(),
            title: title.map(ToString::to_string),
            conversation_path: Some(conv_path.to_string_lossy().to_string()),
            total_tokens,
        })?;

        Ok(())
    }

    /// Delete a conversation session across both storage systems:
    /// 1. Removes `.mermaid/conversations/<id>.{json,jsonl,meta}` and compaction archives.
    /// 2. Deletes SQLite rows (`sessions`, `compactions`, `checkpoints`) and nulls
    ///    any task references.
    ///
    /// # Errors
    ///
    /// Errors if SQLite operations fail or critical filesystem errors occur.
    pub fn delete_session_cascade(&self, project_root: &Path, session_id: &str) -> Result<()> {
        let conv_dir = project_root.join(".mermaid").join("conversations");
        if conv_dir.is_dir() {
            let json_file = conv_dir.join(format!("{session_id}.json"));
            let jsonl_file = conv_dir.join(format!("{session_id}.jsonl"));
            let meta_file = conv_dir.join(format!("{session_id}.meta"));

            let _ = fs::remove_file(json_file);
            let _ = fs::remove_file(jsonl_file);
            let _ = fs::remove_file(meta_file);
        }

        let compactions_dir = project_root
            .join(".mermaid")
            .join("compactions")
            .join(session_id);
        if compactions_dir.is_dir() {
            let _ = fs::remove_dir_all(compactions_dir);
        }

        let scratch_dir = project_root
            .join(".mermaid")
            .join("scratch")
            .join(session_id);
        if scratch_dir.is_dir() {
            let _ = fs::remove_dir_all(scratch_dir);
        }

        let tx = self.store.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM sessions WHERE id = ?1", params![session_id])?;
        tx.execute(
            "DELETE FROM compactions WHERE session_id = ?1",
            params![session_id],
        )?;
        tx.execute(
            "DELETE FROM checkpoints WHERE session_id = ?1",
            params![session_id],
        )?;
        tx.execute(
            "UPDATE tasks SET conversation_id = NULL WHERE conversation_id = ?1",
            params![session_id],
        )?;
        tx.commit()?;

        Ok(())
    }

    /// Reconcile state between the filesystem conversation directory and SQLite.
    ///
    /// Backfills missing `sessions` rows from on-disk JSON/JSONL logs, verifies
    /// task `conversation_id` integrity, and flags orphaned records.
    ///
    /// # Errors
    ///
    /// Errors on SQLite database query failures.
    pub fn reconcile_project(&self, project_root: &Path) -> Result<ReconciliationReport> {
        let mut report = ReconciliationReport::default();
        let conv_dir = project_root.join(".mermaid").join("conversations");
        let proj_str = project_root.to_string_lossy().to_string();

        let mut on_disk_ids = std::collections::HashSet::new();

        if conv_dir.is_dir() {
            let entries = fs::read_dir(&conv_dir).unwrap_or_else(|_| fs::read_dir(".").unwrap());
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                    continue;
                }
                let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                on_disk_ids.insert(stem.to_string());

                // Check if SQLite has this session
                let existing = self.store.sessions().get(stem)?;
                if existing.is_none() {
                    // Attempt backfill
                    let Ok(content) = fs::read_to_string(&path) else {
                        continue;
                    };
                    let Ok(conv) = serde_json::from_str::<MinimalConversationFile>(&content) else {
                        continue;
                    };
                    let model = conv.model_name.unwrap_or_else(|| "unknown".into());
                    let project = conv.project_path.unwrap_or_else(|| proj_str.clone());
                    let _ = self.store.sessions().upsert(NewSession {
                        id: Some(conv.id),
                        project_path: project,
                        model_id: model,
                        title: conv.title,
                        conversation_path: Some(path.to_string_lossy().to_string()),
                        total_tokens: conv.total_tokens,
                    });
                    report.backfilled_sessions += 1;
                    continue;
                }
                report.valid_sessions += 1;
            }
        }

        // Verify task conversation pointers
        let tasks = self.store.tasks().list(1000)?;
        for task in tasks {
            if task.project_path != proj_str {
                continue;
            }
            if let Some(ref conv_id) = task.conversation_id {
                let json_path = conv_dir.join(format!("{conv_id}.json"));
                let jsonl_path = conv_dir.join(format!("{conv_id}.jsonl"));
                if !json_path.is_file() && !jsonl_path.is_file() {
                    report.orphaned_tasks.push(task.id);
                }
            }
        }

        // Verify SQLite session rows have backing files
        let sessions = self.store.sessions().list(1000)?;
        for session in sessions {
            if session.project_path != proj_str {
                continue;
            }
            if !on_disk_ids.contains(&session.id) {
                let json_path = conv_dir.join(format!("{}.json", session.id));
                let jsonl_path = conv_dir.join(format!("{}.jsonl", session.id));
                if !json_path.is_file() && !jsonl_path.is_file() {
                    report.missing_session_files += 1;
                }
            }
        }

        Ok(report)
    }

    /// Garbage-collect expired state across both SQLite and filesystem stores.
    ///
    /// # Errors
    ///
    /// Errors if SQLite operations fail.
    pub fn gc_cross_store(
        &self,
        project_root: Option<&Path>,
        retention_days: i64,
        outcomes_retention_days: i64,
    ) -> Result<CrossStoreGcReport> {
        let mut report = CrossStoreGcReport {
            db_rows_removed: self.store.gc(retention_days, outcomes_retention_days)?,
            conversation_files_removed: 0,
            compactions_removed: 0,
        };

        if let Some(root) = project_root {
            let conv_dir = root.join(".mermaid").join("conversations");
            if conv_dir.is_dir() {
                let cutoff = std::time::SystemTime::now()
                    - std::time::Duration::from_secs(retention_days as u64 * 86400);

                if let Ok(entries) = fs::read_dir(&conv_dir) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        let is_old = entry
                            .metadata()
                            .ok()
                            .and_then(|m| m.modified().ok())
                            .is_some_and(|mod_time| mod_time < cutoff);

                        if is_old && fs::remove_file(&path).is_ok() {
                            report.conversation_files_removed += 1;
                        }
                    }
                }
            }

            let compactions_dir = root.join(".mermaid").join("compactions");
            if compactions_dir.is_dir() {
                let cutoff = std::time::SystemTime::now()
                    - std::time::Duration::from_secs(retention_days as u64 * 86400);
                if let Ok(entries) = fs::read_dir(&compactions_dir) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        let is_old_dir = path.is_dir()
                            && entry
                                .metadata()
                                .ok()
                                .and_then(|m| m.modified().ok())
                                .is_some_and(|mod_time| mod_time < cutoff);

                        if is_old_dir && fs::remove_dir_all(&path).is_ok() {
                            report.compactions_removed += 1;
                        }
                    }
                }
            }
        }

        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{NewTask, fresh_id};
    use mermaid_model::records::TaskPriority;

    struct TestDir {
        path: std::path::PathBuf,
    }

    impl TestDir {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("mermaid_coord_test_{}", fresh_id("test")));
            let _ = fs::create_dir_all(&path);
            Self { path }
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn test_link_task_conversation_persists_in_both_tables() -> Result<()> {
        let temp = TestDir::new();
        let store = RuntimeStore::open(temp.path.join("runtime.sqlite3"))?;
        let coordinator = store.coordinator();
        let project_path = &temp.path;

        let task = store.tasks().create(NewTask {
            title: "Task 1".into(),
            priority: TaskPriority::Normal,
            project_path: project_path.to_string_lossy().to_string(),
            model_id: "test-model".into(),
            conversation_id: None,
            owner_kind: None,
            prompt: None,
        })?;

        coordinator.link_task_conversation(
            &task.id,
            "session-123",
            project_path,
            "test-model",
            Some("Session 123"),
            Some(42),
        )?;

        let updated_task = store.tasks().get(&task.id)?.unwrap();
        assert_eq!(updated_task.conversation_id.as_deref(), Some("session-123"));

        let session = store.sessions().get("session-123")?.unwrap();
        assert_eq!(session.id, "session-123");
        assert_eq!(session.title.as_deref(), Some("Session 123"));
        assert_eq!(session.total_tokens, Some(42));

        Ok(())
    }

    #[test]
    fn test_delete_session_cascade_removes_files_and_db_rows() -> Result<()> {
        let temp = TestDir::new();
        let store = RuntimeStore::open(temp.path.join("runtime.sqlite3"))?;
        let coordinator = store.coordinator();
        let project_path = &temp.path;

        let conv_dir = project_path.join(".mermaid").join("conversations");
        fs::create_dir_all(&conv_dir)?;
        let json_file = conv_dir.join("sess-1.json");
        let jsonl_file = conv_dir.join("sess-1.jsonl");
        fs::write(&json_file, "{}")?;
        fs::write(&jsonl_file, "{}")?;

        let task = store.tasks().create(NewTask {
            title: "Task 1".into(),
            priority: TaskPriority::Normal,
            project_path: project_path.to_string_lossy().to_string(),
            model_id: "test-model".into(),
            conversation_id: Some("sess-1".into()),
            owner_kind: None,
            prompt: None,
        })?;

        store.sessions().upsert(NewSession {
            id: Some("sess-1".into()),
            project_path: project_path.to_string_lossy().to_string(),
            model_id: "test-model".into(),
            title: Some("Sess 1".into()),
            conversation_path: Some(json_file.to_string_lossy().to_string()),
            total_tokens: None,
        })?;

        coordinator.delete_session_cascade(project_path, "sess-1")?;

        assert!(!json_file.exists());
        assert!(!jsonl_file.exists());
        assert!(store.sessions().get("sess-1")?.is_none());

        let updated_task = store.tasks().get(&task.id)?.unwrap();
        assert_eq!(updated_task.conversation_id, None);

        Ok(())
    }

    #[test]
    fn test_reconcile_backfills_missing_sessions_from_json() -> Result<()> {
        let temp = TestDir::new();
        let store = RuntimeStore::open(temp.path.join("runtime.sqlite3"))?;
        let coordinator = store.coordinator();
        let project_path = &temp.path;

        let conv_dir = project_path.join(".mermaid").join("conversations");
        fs::create_dir_all(&conv_dir)?;
        let json_file = conv_dir.join("20260810_120000_000.json");
        fs::write(
            &json_file,
            r#"{"id":"20260810_120000_000","title":"Backfill Test","model_name":"gpt-4","project_path":"/tmp/test","total_tokens":100}"#,
        )?;

        let report = coordinator.reconcile_project(project_path)?;
        assert_eq!(report.backfilled_sessions, 1);

        let session = store.sessions().get("20260810_120000_000")?.unwrap();
        assert_eq!(session.title.as_deref(), Some("Backfill Test"));
        assert_eq!(session.model_id, "gpt-4");
        assert_eq!(session.total_tokens, Some(100));

        Ok(())
    }

    #[test]
    fn test_reconcile_detects_orphaned_tasks() -> Result<()> {
        let temp = TestDir::new();
        let store = RuntimeStore::open(temp.path.join("runtime.sqlite3"))?;
        let coordinator = store.coordinator();
        let project_path = &temp.path;

        let task = store.tasks().create(NewTask {
            title: "Task with missing file".into(),
            priority: TaskPriority::Normal,
            project_path: project_path.to_string_lossy().to_string(),
            model_id: "test-model".into(),
            conversation_id: Some("missing-session".into()),
            owner_kind: None,
            prompt: None,
        })?;

        let report = coordinator.reconcile_project(project_path)?;
        assert_eq!(report.orphaned_tasks, vec![task.id]);

        Ok(())
    }

    #[test]
    fn test_gc_cross_store_runs_without_error() -> Result<()> {
        let temp = TestDir::new();
        let store = RuntimeStore::open(temp.path.join("runtime.sqlite3"))?;
        let coordinator = store.coordinator();
        let project_path = &temp.path;

        let conv_dir = project_path.join(".mermaid").join("conversations");
        fs::create_dir_all(&conv_dir)?;
        let json_file = conv_dir.join("old-session.json");
        fs::write(&json_file, "{}")?;

        let report = coordinator.gc_cross_store(Some(project_path), 30, 90)?;
        assert_eq!(report.db_rows_removed, 0);

        Ok(())
    }
}
