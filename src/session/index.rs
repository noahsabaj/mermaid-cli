//! The cross-project session index in `runtime.sqlite3`.
//!
//! `.mermaid/conversations/<id>.{jsonl,meta,json}` is the truth about a
//! session; the SQLite `sessions` table is an index over those files that the
//! daemon and its dashboard read across projects. Every save upserts the row
//! ([`session_row`] is the one mapping), and the daemon rebuilds the index on
//! start for every project it has ever heard of ([`rebuild_session_index`]),
//! so the table can never disagree with the disk for longer than one daemon
//! restart. There is no manual reconcile step, because a cache does not need
//! one.

use std::path::Path;

use mermaid_domain::ConversationHistory;
use mermaid_runtime::{NewSession, RuntimeStore};

use super::ConversationManager;

/// The index row for a session snapshot. The single mapping from the file's
/// contents to the table's columns.
#[must_use]
pub fn session_row(conversations_dir: &Path, snapshot: &ConversationHistory) -> NewSession {
    let conversation_path = conversations_dir.join(format!("{}.json", snapshot.id));
    NewSession {
        id: Some(snapshot.id.clone()),
        // The snapshot's own field, not the runner's workdir: they are the
        // same string by construction (`State::new` derives it from `cwd`),
        // and one source beats two that must agree.
        project_path: snapshot.project_path.clone(),
        model_id: snapshot.model_name.clone(),
        title: Some(snapshot.title.clone()),
        conversation_path: Some(conversation_path.display().to_string()),
        // Saturating rather than `as`: the column is signed, and a count that
        // somehow exceeded i64 must not land negative.
        total_tokens: Some(
            i64::try_from(snapshot.cumulative_token_usage.total_tokens()).unwrap_or(i64::MAX),
        ),
    }
}

/// What a rebuild changed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SessionIndexReport {
    /// Project directories that exist and were walked.
    pub projects: usize,
    /// Sessions with files on disk that had no row.
    pub backfilled: usize,
    /// Rows whose project directory exists but whose files are gone.
    pub pruned: usize,
}

/// Rebuild the index from disk for every project the store knows: backfill a
/// row for each session that has files but no row, and drop rows whose files
/// are gone. A project directory that does not exist is left alone -- an
/// unmounted drive is indistinguishable from a deleted checkout, and a stale
/// row for an unreachable project costs nothing. Per-project I/O errors are
/// logged and skipped; they never fail the rebuild.
///
/// # Errors
///
/// Errors only if the store itself cannot be queried.
pub fn rebuild_session_index(store: &RuntimeStore) -> anyhow::Result<SessionIndexReport> {
    let mut report = SessionIndexReport::default();
    for project in store.known_project_paths()? {
        let project_dir = Path::new(&project);
        if !project_dir.is_dir() {
            continue;
        }
        report.projects += 1;
        let manager = match ConversationManager::new(project_dir) {
            Ok(manager) => manager,
            Err(error) => {
                tracing::warn!(project = %project, %error, "session index: cannot open project");
                continue;
            },
        };
        let on_disk: Vec<String> = match manager.list_conversation_metas() {
            Ok(metas) => metas.into_iter().map(|meta| meta.id).collect(),
            Err(error) => {
                tracing::warn!(project = %project, %error, "session index: cannot list sessions");
                continue;
            },
        };
        let indexed = store.sessions().ids_for_project(&project)?;
        for id in &on_disk {
            if indexed.contains(id) {
                continue;
            }
            let snapshot = match manager.load_conversation(id) {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    tracing::warn!(project = %project, session = %id, %error, "session index: cannot load session");
                    continue;
                },
            };
            store
                .sessions()
                .upsert(session_row(manager.conversations_dir(), &snapshot))?;
            report.backfilled += 1;
        }
        for id in indexed {
            if on_disk.contains(&id) {
                continue;
            }
            if store.sessions().delete(&id)? {
                report.pruned += 1;
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Local;
    use mermaid_model::models::ChatMessage;
    use mermaid_runtime::NewTask;

    fn temp_store(name: &str) -> RuntimeStore {
        let dir = std::env::temp_dir().join(format!(
            "mermaid_session_index_{name}_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        RuntimeStore::open(dir.join("runtime.sqlite3")).unwrap()
    }

    fn temp_project(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mermaid_session_index_proj_{name}_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A saved session whose files exist on disk, with a task row that names
    /// the project (which is how the store comes to know the project at all).
    fn saved_session(project: &std::path::Path) -> (ConversationManager, ConversationHistory) {
        let manager = ConversationManager::new(project).unwrap();
        let mut conv = ConversationHistory::new(
            project.display().to_string(),
            "test/model".into(),
            Local::now(),
        );
        conv.add_messages(&[ChatMessage::user("hi")], Local::now());
        // The append path is what a live session writes: the `.jsonl` log
        // (backfilled from the snapshot here) and the `.meta` sidecar. The
        // `.json` checkpoint is a throttled cache and is deliberately absent.
        manager.append_session_events(&conv, &[]).unwrap();
        (manager, conv)
    }

    /// The case the old reconciler skipped: a session whose `.jsonl` and
    /// `.meta` exist but whose `.json` checkpoint does not (a short session, or
    /// an unclean exit) must still be backfilled.
    #[test]
    fn rebuild_backfills_a_session_that_has_files_but_no_row() {
        let store = temp_store("backfill");
        let project = temp_project("backfill");
        let (manager, conv) = saved_session(&project);
        assert!(
            !manager
                .conversations_dir()
                .join(format!("{}.json", conv.id))
                .exists()
        );
        store
            .tasks()
            .create(NewTask::new(
                "t",
                project.display().to_string(),
                "test/model",
            ))
            .unwrap();
        assert!(store.sessions().get(&conv.id).unwrap().is_none());

        let report = rebuild_session_index(&store).unwrap();
        assert_eq!(report.backfilled, 1, "{report:?}");
        assert_eq!(report.pruned, 0);
        let row = store
            .sessions()
            .get(&conv.id)
            .unwrap()
            .expect("backfilled row");
        assert_eq!(row.model_id, "test/model");
        assert_eq!(row.project_path, project.display().to_string());
        let _ = std::fs::remove_dir_all(&project);
    }

    #[test]
    fn rebuild_prunes_a_row_whose_files_are_gone() {
        let store = temp_store("prune");
        let project = temp_project("prune");
        store
            .sessions()
            .upsert(NewSession {
                id: Some("20260101_000000_gone".to_string()),
                project_path: project.display().to_string(),
                model_id: "m".to_string(),
                title: None,
                conversation_path: None,
                total_tokens: None,
            })
            .unwrap();
        let report = rebuild_session_index(&store).unwrap();
        assert_eq!(report.pruned, 1, "{report:?}");
        assert!(
            store
                .sessions()
                .get("20260101_000000_gone")
                .unwrap()
                .is_none()
        );
        let _ = std::fs::remove_dir_all(&project);
    }

    #[test]
    fn rebuild_leaves_rows_of_unreachable_projects_alone() {
        let store = temp_store("unreachable");
        store
            .sessions()
            .upsert(NewSession {
                id: Some("20260101_000000_far".to_string()),
                project_path: "/definitely/not/mounted/here".to_string(),
                model_id: "m".to_string(),
                title: None,
                conversation_path: None,
                total_tokens: None,
            })
            .unwrap();
        let report = rebuild_session_index(&store).unwrap();
        assert_eq!(report, SessionIndexReport::default());
        assert!(
            store
                .sessions()
                .get("20260101_000000_far")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn session_row_is_the_single_index_mapping() {
        let project = temp_project("row");
        let (manager, conv) = saved_session(&project);
        let row = session_row(manager.conversations_dir(), &conv);
        assert_eq!(row.id.as_deref(), Some(conv.id.as_str()));
        assert_eq!(row.project_path, conv.project_path);
        assert_eq!(row.model_id, "test/model");
        assert_eq!(row.title.as_deref(), Some(conv.title.as_str()));
        assert!(
            row.conversation_path
                .unwrap()
                .ends_with(&format!("{}.json", conv.id))
        );
        let _ = std::fs::remove_dir_all(&project);
    }
}
