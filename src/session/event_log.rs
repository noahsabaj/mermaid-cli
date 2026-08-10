//! The per-session event-log appender and fold-reader.
//!
//! Design: `docs/design/event-log.md` (PR C). One JSONL file per session at
//! `.mermaid/conversations/<id>.jsonl` — invisible to every listing path
//! (they filter on the `json` extension) and cascaded by
//! `delete_conversation`. The snapshot stays the resume authority; this file
//! is the history behind it and the recovery source when the snapshot is
//! lost or corrupt.
//!
//! This module is the ONE disk chokepoint for events, owning the same
//! properties the snapshot writer owns for its file: session-id validation
//! before any path join, credential redaction per line, screenshot
//! stripping on message-bearing events (and dropping `image` events
//! outright — a screenshot must not reach durable storage, #99), owner-only
//! create mode, and a read cap. A failed append evicts the cached `seq`
//! cursor so the next save re-derives it from disk — the same self-healing
//! posture as `with_shared_store` on the flaky-drive case.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use anyhow::{Context, Result};
use mermaid_domain::{
    ConversationHistory, SESSION_EVENT_FORMAT_VERSION, SessionEvent, SessionEventLine,
    SessionScalars, fold_session,
};
use mermaid_model::models::{ChatMessage, MessageRole};

/// Upper bound on a log file we will read back for a fold, mirroring the
/// snapshot's 64 MiB cap with headroom for the event envelopes.
const MAX_LOG_BYTES: u64 = 128 * 1024 * 1024;

/// Marker appended to a message's text when its screenshot bytes are
/// dropped at the append chokepoint — the same marker the snapshot writer
/// uses, so a fold and a stored snapshot describe the strip identically.
const SCREENSHOT_ELIDED_MARKER: &str = "\n[screenshot not persisted]";

/// Appender + reader for the per-session `.jsonl` logs of one
/// conversations directory. Shared behind the manager's `Arc`, so clones
/// see one `seq` cursor per session within a process; separate processes
/// re-derive the cursor from the file, and interleaved appends stay
/// line-atomic enough for the prefix-fold reader.
pub struct EventLog {
    dir: PathBuf,
    /// Next `seq` per session id, as THIS process last knew it. Populated
    /// by scanning the file on first touch; evicted on any append error so
    /// a transient failure cannot wedge the cursor.
    next_seq: Mutex<HashMap<String, u64>>,
}

impl EventLog {
    pub(crate) fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            next_seq: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn path_for(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.jsonl"))
    }

    /// Append `events` for `snapshot`'s session, creating the log on first
    /// touch. The caller validates the session id before this runs.
    ///
    /// Creation writes the snapshot BACKFILL alone and drops `events`: the
    /// snapshot already contains those events' effects, so writing both
    /// would double-count them in a fold. Only an existing log takes the
    /// granular batch. A message-less session gets no log, mirroring the
    /// snapshot writer's empty-session guard.
    pub(crate) fn append(
        &self,
        snapshot: &ConversationHistory,
        events: &[SessionEvent],
    ) -> Result<()> {
        if snapshot.messages().is_empty() {
            return Ok(());
        }
        let path = self.path_for(&snapshot.id);
        let result = self.append_inner(&path, snapshot, events);
        if result.is_err() {
            // Self-heal: drop the cached cursor so the next save re-derives
            // it from whatever actually reached disk.
            self.next_seq
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&snapshot.id);
        }
        result
    }

    fn append_inner(
        &self,
        path: &Path,
        snapshot: &ConversationHistory,
        events: &[SessionEvent],
    ) -> Result<()> {
        let creating = !path.exists();
        let backfill = if creating {
            backfill_events(snapshot)
        } else {
            Vec::new()
        };
        let batch: &[SessionEvent] = if creating { &backfill } else { events };
        if batch.is_empty() {
            return Ok(());
        }

        // A fresh file starts at 0 whatever the cache thinks (the file may
        // have been deleted out from under a live process).
        let mut seq = if creating {
            0
        } else {
            self.next_seq_for(&snapshot.id, path)?
        };
        let mut file = open_append(path)?;
        let ts = chrono::Local::now();
        for event in batch {
            let Some(event) = sanitize_event(event) else {
                continue;
            };
            let line = SessionEventLine {
                v: SESSION_EVENT_FORMAT_VERSION,
                seq,
                ts,
                event,
            };
            let mut value = serde_json::to_value(&line).context("serialize session event")?;
            // The one redaction pass for this store: a credential that
            // crossed the transcript must not reach the log in cleartext,
            // exactly as the snapshot writer scrubs its file (#17).
            mermaid_model::utils::redact_json(&mut value);
            writeln!(file, "{value}").context("append session event line")?;
            seq += 1;
        }
        file.flush().context("flush session event log")?;
        self.next_seq
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(snapshot.id.clone(), seq);
        Ok(())
    }

    /// The next `seq` for `id`: the cached cursor, or a one-time scan of
    /// the existing file (line count) on first touch.
    fn next_seq_for(&self, id: &str, path: &Path) -> Result<u64> {
        if let Some(seq) = self
            .next_seq
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
        {
            return Ok(*seq);
        }
        if !path.exists() {
            return Ok(0);
        }
        let file = File::open(path)
            .with_context(|| format!("open {} to derive the seq cursor", path.display()))?;
        Ok(BufReader::new(file).lines().count() as u64)
    }

    /// Rebuild the conversation from the log. `Ok(None)` when there is no
    /// log, it is over the size cap, its format is newer than this build,
    /// or it does not begin with a `started` event. A malformed or torn
    /// line ends the fold THERE (prefix semantics): a crash mid-append
    /// yields the pre-tear state instead of a refusal.
    pub(crate) fn fold(&self, id: &str) -> Result<Option<ConversationHistory>> {
        let path = self.path_for(id);
        let Ok(meta) = std::fs::metadata(&path) else {
            return Ok(None);
        };
        if meta.len() > MAX_LOG_BYTES {
            tracing::warn!(path = %path.display(), "session event log over the read cap; not folding");
            return Ok(None);
        }
        let file =
            File::open(&path).with_context(|| format!("open {} for fold", path.display()))?;
        let mut events = Vec::new();
        for line in BufReader::new(file).lines() {
            let raw = line.context("read session event line")?;
            let parsed: SessionEventLine = match serde_json::from_str(&raw) {
                Ok(parsed) => parsed,
                Err(error) => {
                    tracing::warn!(path = %path.display(), %error, "malformed session event line; folding the prefix");
                    break;
                },
            };
            if parsed.v > SESSION_EVENT_FORMAT_VERSION {
                tracing::warn!(
                    path = %path.display(),
                    version = parsed.v,
                    "session event log written by a newer mermaid; not folding"
                );
                return Ok(None);
            }
            events.push(parsed.event);
        }
        Ok(fold_session(events))
    }
}

/// The synthetic first lines of a freshly created log: identity, the whole
/// transcript in one `reset` (stamped with the snapshot's `updated_at`, so
/// the fold's clock matches), the prompt history, and the scalar/checklist
/// baselines. Subsumes any pending events by construction — the snapshot
/// is the AFTER state. Pre-existing `compactions` METADATA is the one
/// thing a fold of a backfilled log cannot carry — the snapshot remains
/// authoritative for it, and recovery of a lost snapshot loses only that
/// bookkeeping, never messages.
fn backfill_events(snapshot: &ConversationHistory) -> Vec<SessionEvent> {
    let mut events = vec![SessionEvent::Started {
        session_id: snapshot.id.clone(),
        project_path: snapshot.project_path.clone(),
        model_id: snapshot.model_name.clone(),
        created_at: snapshot.created_at,
        forked_from: snapshot.forked_from.clone(),
        parent_session: snapshot.parent_session.clone(),
    }];
    if !snapshot.messages().is_empty() {
        events.push(SessionEvent::Reset {
            at: snapshot.updated_at,
            messages: snapshot.messages().to_vec(),
        });
    }
    for text in &snapshot.input_history {
        events.push(SessionEvent::Input { text: text.clone() });
    }
    events.push(SessionEvent::State(Box::new(SessionScalars::of(snapshot))));
    events.push(SessionEvent::Tasks {
        store: snapshot.tasks.clone(),
    });
    events
}

/// Screenshot policy at the disk boundary (#99), matching the snapshot
/// writer's: images on non-User messages are dropped (with the marker
/// appended), and standalone `image` events — always a tool screenshot
/// routed onto an assistant message — are dropped whole. User-supplied
/// images are intentional content and pass through.
fn sanitize_event(event: &SessionEvent) -> Option<SessionEvent> {
    match event {
        SessionEvent::Image { .. } => None,
        SessionEvent::Message { message } => Some(SessionEvent::Message {
            message: strip_screenshot(message),
        }),
        SessionEvent::InsertedBeforeLast { message } => Some(SessionEvent::InsertedBeforeLast {
            message: strip_screenshot(message),
        }),
        SessionEvent::Compaction {
            at,
            record,
            replacement,
        } => Some(SessionEvent::Compaction {
            at: *at,
            record: record.clone(),
            replacement: replacement.iter().map(strip_screenshot).collect(),
        }),
        SessionEvent::Reset { at, messages } => Some(SessionEvent::Reset {
            at: *at,
            messages: messages.iter().map(strip_screenshot).collect(),
        }),
        // Carry-through variants, listed rather than wildcarded so a new
        // event type has to state its screenshot policy here.
        event @ (SessionEvent::Started { .. }
        | SessionEvent::Action { .. }
        | SessionEvent::State(_)
        | SessionEvent::Input { .. }
        | SessionEvent::Tasks { .. }) => Some(event.clone()),
    }
}

fn strip_screenshot(message: &ChatMessage) -> ChatMessage {
    if message.role == MessageRole::User || message.images.is_none() {
        return message.clone();
    }
    let mut stripped = message.clone();
    stripped.images = None;
    if !stripped.content.ends_with(SCREENSHOT_ELIDED_MARKER) {
        stripped.content.push_str(SCREENSHOT_ELIDED_MARKER);
    }
    stripped
}

/// Open (creating 0600 if needed) the log for append. The log carries the
/// transcript in cleartext minus redaction, so it gets the same owner-only
/// posture as the snapshot and the recorder (#132).
fn open_append(path: &Path) -> Result<File> {
    let mut opts = OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
        .with_context(|| format!("open {} for event append", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::ConversationManager;
    use mermaid_domain::{Config, State};

    fn fixed_ts() -> chrono::DateTime<chrono::Local> {
        chrono::DateTime::parse_from_rfc3339("2026-07-02T12:00:00.123+00:00")
            .unwrap()
            .with_timezone(&chrono::Local)
    }

    fn temp_root(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("mermaid_event_log_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// A driven session: user prompt, assistant reply, input history, one
    /// scalar change — drained the way the reducer drains.
    fn driven_state(root: &Path) -> State {
        let mut state = State::new(
            Config::default(),
            root.to_path_buf(),
            "ollama/test".to_string(),
            fixed_ts(),
            std::env::temp_dir(),
        );
        state
            .session
            .append(ChatMessage::user("hello event log"), fixed_ts());
        state.session.record_input("hello event log".to_string());
        state
            .session
            .append(ChatMessage::assistant("hi there"), fixed_ts());
        state
    }

    fn json_of(c: &ConversationHistory) -> serde_json::Value {
        serde_json::to_value(c).expect("serializes")
    }

    #[test]
    fn append_then_fold_equals_the_loaded_snapshot() {
        let root = temp_root("roundtrip");
        let manager = ConversationManager::new(&root).unwrap();
        let mut state = driven_state(&root);
        let snapshot = state.session.snapshot_conversation();
        let events = state.session.drain_events(&snapshot);

        manager.append_session_events(&snapshot, &events).unwrap();
        manager.save_conversation(&snapshot).unwrap();

        let loaded = manager.load_conversation(&snapshot.id).unwrap();
        let folded = manager
            .fold_conversation_from_log(&snapshot.id)
            .unwrap()
            .expect("log folds");
        assert_eq!(
            json_of(&folded),
            json_of(&loaded),
            "fold(disk log) must equal the loaded snapshot"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn corrupt_snapshot_recovers_from_the_log() {
        let root = temp_root("recovery");
        let manager = ConversationManager::new(&root).unwrap();
        let mut state = driven_state(&root);
        let snapshot = state.session.snapshot_conversation();
        let events = state.session.drain_events(&snapshot);
        manager.append_session_events(&snapshot, &events).unwrap();
        manager.save_conversation(&snapshot).unwrap();

        // Corrupt the snapshot the way a torn write would.
        let json_path = manager
            .conversations_dir()
            .join(format!("{}.json", snapshot.id));
        std::fs::write(&json_path, b"{ torn").unwrap();

        let recovered = manager
            .load_conversation(&snapshot.id)
            .expect("recovery must kick in");
        assert_eq!(recovered.id, snapshot.id);
        assert_eq!(recovered.messages().len(), 2);
        assert_eq!(recovered.title, snapshot.title);

        // A MISSING snapshot recovers too.
        std::fs::remove_file(&json_path).unwrap();
        let recovered = manager
            .load_conversation(&snapshot.id)
            .expect("missing snapshot recovers");
        assert_eq!(recovered.messages().len(), 2);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn backfill_materializes_once_and_folds_exactly() {
        let root = temp_root("backfill");
        let manager = ConversationManager::new(&root).unwrap();
        // A pre-log session: saved snapshot, no `.jsonl` (pre-upgrade).
        let mut state = driven_state(&root);
        let snapshot = state.session.snapshot_conversation();
        // Deliberately DROP the drained events — this session predates logs.
        let _ = state.session.drain_events(&snapshot);
        manager.save_conversation(&snapshot).unwrap();
        let log_path = manager
            .conversations_dir()
            .join(format!("{}.jsonl", snapshot.id));
        assert!(!log_path.exists());

        // First append (even empty) backfills from the snapshot.
        manager.append_session_events(&snapshot, &[]).unwrap();
        assert!(log_path.exists());
        let lines_after_backfill = std::fs::read_to_string(&log_path).unwrap().lines().count();
        let folded = manager
            .fold_conversation_from_log(&snapshot.id)
            .unwrap()
            .expect("backfill folds");
        assert_eq!(
            json_of(&folded),
            json_of(&manager.load_conversation(&snapshot.id).unwrap()),
            "a compaction-free backfill folds to the snapshot exactly"
        );

        // A second (empty) append is a no-op — no duplicate backfill.
        manager.append_session_events(&snapshot, &[]).unwrap();
        assert_eq!(
            std::fs::read_to_string(&log_path).unwrap().lines().count(),
            lines_after_backfill
        );

        // And a real append continues the seq from the backfilled lines.
        state
            .session
            .append(ChatMessage::user("post-upgrade"), fixed_ts());
        let snapshot = state.session.snapshot_conversation();
        let events = state.session.drain_events(&snapshot);
        manager.append_session_events(&snapshot, &events).unwrap();
        let raw = std::fs::read_to_string(&log_path).unwrap();
        let seqs: Vec<u64> = raw
            .lines()
            .map(|l| serde_json::from_str::<SessionEventLine>(l).unwrap().seq)
            .collect();
        let expected: Vec<u64> = (0..seqs.len() as u64).collect();
        assert_eq!(seqs, expected, "seq is gapless and monotonic");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn seq_continues_across_manager_instances() {
        let root = temp_root("seq_scan");
        let manager = ConversationManager::new(&root).unwrap();
        let mut state = driven_state(&root);
        let snapshot = state.session.snapshot_conversation();
        let events = state.session.drain_events(&snapshot);
        manager.append_session_events(&snapshot, &events).unwrap();

        // A separate process (fresh manager, empty seq cache) appends more:
        // the cursor must be re-derived from the file, not restart at 0.
        let other = ConversationManager::new(&root).unwrap();
        state
            .session
            .append(ChatMessage::user("second process"), fixed_ts());
        let snapshot = state.session.snapshot_conversation();
        let more = state.session.drain_events(&snapshot);
        other.append_session_events(&snapshot, &more).unwrap();

        let raw = std::fs::read_to_string(
            manager
                .conversations_dir()
                .join(format!("{}.jsonl", snapshot.id)),
        )
        .unwrap();
        let seqs: Vec<u64> = raw
            .lines()
            .map(|l| serde_json::from_str::<SessionEventLine>(l).unwrap().seq)
            .collect();
        let expected: Vec<u64> = (0..seqs.len() as u64).collect();
        assert_eq!(seqs, expected);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn screenshots_and_secrets_never_reach_the_log() {
        let root = temp_root("sanitize");
        let manager = ConversationManager::new(&root).unwrap();
        let mut state = driven_state(&root);
        // A tool screenshot lands on the assistant message + its event.
        state.session.attach_image("SHOTBYTES64".to_string());
        // A credential crosses the transcript.
        state.session.append(
            ChatMessage::assistant("OPENAI_API_KEY=sk-abcdefghijklmnop1234"),
            fixed_ts(),
        );
        let snapshot = state.session.snapshot_conversation();
        let events = state.session.drain_events(&snapshot);
        manager.append_session_events(&snapshot, &events).unwrap();

        let raw = std::fs::read_to_string(
            manager
                .conversations_dir()
                .join(format!("{}.jsonl", snapshot.id)),
        )
        .unwrap();
        assert!(!raw.contains("SHOTBYTES64"), "screenshot leaked: {raw}");
        assert!(
            !raw.contains("sk-abcdefghijklmnop1234"),
            "secret leaked: {raw}"
        );
        assert!(raw.contains("[REDACTED]"), "redaction marker expected");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn torn_tail_line_folds_the_prefix() {
        let root = temp_root("torn");
        let manager = ConversationManager::new(&root).unwrap();
        let mut state = driven_state(&root);
        let snapshot = state.session.snapshot_conversation();
        let events = state.session.drain_events(&snapshot);
        manager.append_session_events(&snapshot, &events).unwrap();

        let log_path = manager
            .conversations_dir()
            .join(format!("{}.jsonl", snapshot.id));
        let mut raw = std::fs::read_to_string(&log_path).unwrap();
        raw.push_str("{\"v\":1,\"seq\":99,\"ts\":\"2026-"); // torn mid-write
        std::fs::write(&log_path, raw).unwrap();

        let folded = manager
            .fold_conversation_from_log(&snapshot.id)
            .unwrap()
            .expect("prefix folds");
        assert_eq!(folded.messages().len(), 2, "the pre-tear state survives");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn newer_format_log_is_refused_not_misread() {
        let root = temp_root("newer");
        let manager = ConversationManager::new(&root).unwrap();
        let id = "20260101_120000_001";
        std::fs::write(
            manager.conversations_dir().join(format!("{id}.jsonl")),
            format!(
                "{{\"v\":{},\"seq\":0,\"ts\":\"2026-07-02T12:00:00.123+00:00\",\"event\":{{\"type\":\"started\",\"session_id\":\"{id}\",\"project_path\":\"/p\",\"model_id\":\"m\",\"created_at\":\"2026-07-02T12:00:00.123+00:00\"}}}}\n",
                SESSION_EVENT_FORMAT_VERSION + 1
            ),
        )
        .unwrap();
        assert!(
            manager.fold_conversation_from_log(id).unwrap().is_none(),
            "a newer-format log must not be folded"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn delete_conversation_removes_the_log_too() {
        let root = temp_root("delete");
        let manager = ConversationManager::new(&root).unwrap();
        let mut state = driven_state(&root);
        let snapshot = state.session.snapshot_conversation();
        let events = state.session.drain_events(&snapshot);
        manager.append_session_events(&snapshot, &events).unwrap();
        manager.save_conversation(&snapshot).unwrap();
        let log_path = manager
            .conversations_dir()
            .join(format!("{}.jsonl", snapshot.id));
        assert!(log_path.exists());
        manager.delete_conversation(&snapshot.id).unwrap();
        assert!(!log_path.exists(), "the log must cascade with the session");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn log_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_root("perms");
        let manager = ConversationManager::new(&root).unwrap();
        let mut state = driven_state(&root);
        let snapshot = state.session.snapshot_conversation();
        let events = state.session.drain_events(&snapshot);
        manager.append_session_events(&snapshot, &events).unwrap();
        let mode = std::fs::metadata(
            manager
                .conversations_dir()
                .join(format!("{}.jsonl", snapshot.id)),
        )
        .unwrap()
        .permissions()
        .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "log must be created owner-only");
        let _ = std::fs::remove_dir_all(&root);
    }
}
