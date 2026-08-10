//! The per-session event-log appender and fold-reader.
//!
//! Design: `docs/design/event-log.md`, then `docs/design/fold-first-resume.md`.
//! One JSONL file per session at `.mermaid/conversations/<id>.jsonl`, cascaded
//! by `delete_conversation`.
//!
//! This file is the session's TRUTH. The `<id>.json` beside it is a
//! checkpoint: a fold materialized at a known offset, stamped with the last
//! `seq` it contains, and resume replays only what came after. Anything that
//! makes the checkpoint untrustworthy falls back to folding this log from
//! zero, which is the property the arrangement exists for — the checkpoint is
//! never load-bearing, only faster.
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
    /// Creation writes the snapshot BACKFILL, then only the batch's
    /// IDEMPOTENT events. The distinction matters: an additive event
    /// (`message`, `action`, …) is already folded into the snapshot, so
    /// writing it after the backfill would double it — while an assigning
    /// event (`compaction`, `reset`, `state`, `tasks`) restates what the
    /// backfill already says and merely carries a fact the snapshot's
    /// transcript cannot, such as a compaction boundary. Dropping those too
    /// silently lost the boundary whenever a compaction created the log.
    ///
    /// A message-less session gets no log, mirroring the snapshot writer's
    /// empty-session guard.
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
        let batch: Vec<SessionEvent> = if creating {
            let mut seeded = backfill_events(snapshot);
            seeded.extend(events.iter().filter(|e| is_idempotent(e)).cloned());
            seeded
        } else {
            events.to_vec()
        };
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
        for event in &batch {
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
    ///
    /// The scan counts line terminators rather than draining
    /// `BufRead::lines()`, because that iterator does NOT stop at an I/O
    /// error — it yields `Err` and keeps going, so a path whose every read
    /// fails spins forever instead of failing. Linux reaches exactly that
    /// state when the path is a directory: `File::open` succeeds there and
    /// every subsequent read returns `EISDIR`.
    fn next_seq_for(&self, id: &str, path: &Path) -> Result<u64> {
        if let Some(seq) = self
            .next_seq
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
        {
            return Ok(*seq);
        }
        let Ok(meta) = std::fs::metadata(path) else {
            return Ok(0);
        };
        anyhow::ensure!(
            meta.is_file(),
            "session event log path {} is not a file",
            path.display()
        );
        let file = File::open(path)
            .with_context(|| format!("open {} to derive the seq cursor", path.display()))?;
        let mut reader = BufReader::new(file);
        let mut line = Vec::new();
        let mut seq = 0u64;
        loop {
            line.clear();
            let read = reader
                .read_until(b'\n', &mut line)
                .with_context(|| format!("scan {} for the seq cursor", path.display()))?;
            if read == 0 {
                return Ok(seq);
            }
            seq += 1;
        }
    }

    /// Does this session have a log at all? Distinguishes a session whose
    /// truth lives in the log from a legacy one that predates it and has
    /// only a snapshot.
    pub(crate) fn exists(&self, id: &str) -> bool {
        self.path_for(id).is_file()
    }

    /// The last `seq` this process appended for `id`, i.e. the watermark a
    /// checkpoint written right now would carry.
    ///
    /// `None` when this process has not appended for this session, and
    /// deliberately so: the cursor would then be a guess derived from a
    /// file another process may be writing, and a checkpoint stamped with a
    /// guessed watermark is worse than one with no watermark at all — the
    /// first is trusted, the second falls back to a full fold.
    pub(crate) fn checkpoint_seq(&self, id: &str) -> Option<u64> {
        self.next_seq
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
            .and_then(|next| next.checked_sub(1))
    }

    /// Read the log's events, optionally skipping everything at or below
    /// `after_seq` — the checkpoint-replay half of fold-first resume.
    ///
    /// Returns `(events, highest_seq_seen)`. `None` when there is no log,
    /// it is over the size cap, or its format is newer than this build.
    /// A malformed or torn line ends the read THERE (prefix semantics): a
    /// crash mid-append yields the pre-tear state instead of a refusal.
    pub(crate) fn read_events(
        &self,
        id: &str,
        after_seq: Option<u64>,
    ) -> Result<Option<(Vec<SessionEvent>, u64)>> {
        let path = self.path_for(id);
        let Ok(meta) = std::fs::metadata(&path) else {
            return Ok(None);
        };
        // Not a file means there is nothing to read. Worth stating: on Linux
        // a directory here would open cleanly and then fail every read.
        if !meta.is_file() {
            return Ok(None);
        }
        if meta.len() > MAX_LOG_BYTES {
            tracing::warn!(path = %path.display(), "session event log over the read cap; not reading");
            return Ok(None);
        }
        let file =
            File::open(&path).with_context(|| format!("open {} for fold", path.display()))?;
        let mut events = Vec::new();
        let mut highest = 0u64;
        for line in BufReader::new(file).lines() {
            let raw = line.context("read session event line")?;
            let parsed: SessionEventLine = match serde_json::from_str(&raw) {
                Ok(parsed) => parsed,
                Err(error) => {
                    tracing::warn!(path = %path.display(), %error, "malformed session event line; reading the prefix only");
                    break;
                },
            };
            if parsed.v > SESSION_EVENT_FORMAT_VERSION {
                tracing::warn!(
                    path = %path.display(),
                    version = parsed.v,
                    "session event log written by a newer mermaid; not reading"
                );
                return Ok(None);
            }
            highest = highest.max(parsed.seq);
            // The skip happens AFTER the version and parse checks, so a
            // replay past a watermark still refuses a log it cannot read
            // rather than silently returning a short tail.
            if after_seq.is_some_and(|seq| parsed.seq <= seq) {
                continue;
            }
            events.push(parsed.event);
        }
        Ok(Some((events, highest)))
    }

    /// Rebuild the conversation by folding the whole log from zero.
    /// `Ok(None)` when there is no readable log, or it does not begin with
    /// a `started` event.
    pub(crate) fn fold(&self, id: &str) -> Result<Option<ConversationHistory>> {
        let Some((events, _)) = self.read_events(id, None)? else {
            return Ok(None);
        };
        Ok(fold_session(events))
    }

    /// Bring `checkpoint` — a fold of this log up to and including
    /// `checkpoint_seq` — up to date by replaying everything after it.
    ///
    /// `Ok(None)` means the checkpoint cannot be trusted for this log and
    /// the caller should fold from zero instead: either the log is
    /// unreadable, or its highest seq is BELOW the watermark, which says
    /// the log was truncated or replaced since the checkpoint was written.
    /// Being wrong in that direction would silently resume a conversation
    /// containing events that no longer exist.
    pub(crate) fn replay_onto(
        &self,
        id: &str,
        mut checkpoint: ConversationHistory,
        checkpoint_seq: u64,
    ) -> Result<Option<ConversationHistory>> {
        let Some((events, highest)) = self.read_events(id, Some(checkpoint_seq))? else {
            return Ok(None);
        };
        if highest < checkpoint_seq {
            tracing::warn!(
                id,
                checkpoint_seq,
                highest,
                "checkpoint is ahead of its log; folding from zero instead"
            );
            return Ok(None);
        }
        mermaid_domain::replay_events(&mut checkpoint, events);
        Ok(Some(checkpoint))
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

/// Does folding this event ASSIGN state (so replaying it after a backfill
/// of the same snapshot is a no-op) rather than ADD to it? Exhaustive on
/// purpose: a new event type must state which side it is on, because
/// getting it wrong either doubles content or loses a fact.
const fn is_idempotent(event: &SessionEvent) -> bool {
    match event {
        SessionEvent::Compaction { .. }
        | SessionEvent::Reset { .. }
        | SessionEvent::State(_)
        | SessionEvent::Tasks { .. } => true,
        SessionEvent::Started { .. }
        | SessionEvent::Message { .. }
        | SessionEvent::InsertedBeforeLast { .. }
        | SessionEvent::Action { .. }
        | SessionEvent::Image { .. }
        | SessionEvent::Input { .. } => false,
    }
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
    fn creating_a_log_keeps_idempotent_events_and_drops_additive_ones() {
        // The case that motivated the rule: a compaction is the FIRST thing
        // to touch a session's log (an earlier append failed, or the file
        // was removed). Dropping the whole batch lost the boundary, which is
        // the one fact the stripped snapshot cannot carry.
        let root = temp_root("idempotent_carry");
        let manager = ConversationManager::new(&root).unwrap();
        let mut state = driven_state(&root);
        let snapshot = state.session.snapshot_conversation();
        let mut events = state.session.drain_events(&snapshot);
        events.push(SessionEvent::Compaction {
            at: fixed_ts(),
            record: mermaid_domain::CompactionEvent {
                id: "c-first".to_string(),
                trigger: mermaid_domain::CompactionTrigger::Manual,
                created_at: fixed_ts(),
                before_tokens: 100,
                after_tokens: 10,
                archived_message_count: 4,
                preserved_message_count: 2,
                preserved_turn_count: 1,
                summary_tokens: 5,
                duration_secs: 0.1,
                review_status: mermaid_domain::CompactionReviewStatus::Reviewed,
                review_error: None,
                focus: None,
                archive_path: None,
            },
            replacement: snapshot.messages().to_vec(),
        });
        // The batch also carries the additive events for the same messages.
        assert!(
            events
                .iter()
                .any(|e| matches!(e, SessionEvent::Message { .. })),
            "the fixture must carry additive events for this to prove anything"
        );

        manager.append_session_events(&snapshot, &events).unwrap();
        let folded = manager
            .fold_conversation_from_log(&snapshot.id)
            .unwrap()
            .expect("folds");
        assert_eq!(
            folded.messages().len(),
            snapshot.messages().len(),
            "additive events must not double the backfilled transcript"
        );
        assert_eq!(
            folded.compactions.len(),
            1,
            "the compaction boundary must survive log creation"
        );
        assert_eq!(folded.compactions[0].id, "c-first");
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

    /// Drive a session, save it, then append more WITHOUT re-saving — the
    /// state a checkpoint-plus-tail resume exists to handle.
    fn checkpointed_then_advanced(root: &Path) -> (ConversationManager, ConversationHistory) {
        let manager = ConversationManager::new(root).expect("manager");
        let mut state = driven_state(root);
        let checkpointed = state.session.snapshot_conversation();
        let events = state.session.drain_events(&checkpointed);
        manager
            .append_session_events(&checkpointed, &events)
            .expect("append");
        manager
            .save_conversation(&checkpointed)
            .expect("checkpoint");

        // Two more messages that exist only in the log.
        state
            .session
            .append(ChatMessage::user("after the checkpoint"), fixed_ts());
        state
            .session
            .append(ChatMessage::assistant("still here"), fixed_ts());
        let latest = state.session.snapshot_conversation();
        let tail = state.session.drain_events(&latest);
        manager
            .append_session_events(&latest, &tail)
            .expect("append tail");
        (manager, latest)
    }

    #[test]
    fn resume_replays_the_tail_past_the_checkpoint() {
        let root = temp_root("replay_tail");
        let (manager, latest) = checkpointed_then_advanced(&root);

        let resumed = manager.load_conversation(&latest.id).expect("resume");
        assert_eq!(
            json_of(&resumed),
            json_of(&latest),
            "resume must equal the live session, not the stale checkpoint"
        );
        assert!(
            resumed
                .messages()
                .iter()
                .any(|m| m.content == "after the checkpoint"),
            "the events past the checkpoint must be replayed"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resume_ignores_a_checkpoint_it_cannot_place_in_the_log() {
        let root = temp_root("untrusted_ckpt");
        let (manager, latest) = checkpointed_then_advanced(&root);
        let path = manager
            .conversations_dir()
            .join(format!("{}.json", latest.id));

        // A checkpoint with no watermark cannot be placed against the log,
        // so resume must fold from zero rather than trust it. Strip the key
        // and leave the (stale, two-messages-short) body.
        let raw = std::fs::read_to_string(&path).expect("read checkpoint");
        let mut value: serde_json::Value = serde_json::from_str(&raw).expect("parse");
        assert!(
            value
                .as_object_mut()
                .expect("object")
                .remove("checkpoint_seq")
                .is_some(),
            "the checkpoint must have carried a watermark for this to prove anything"
        );
        std::fs::write(&path, serde_json::to_string(&value).unwrap()).expect("rewrite");

        let resumed = manager.load_conversation(&latest.id).expect("resume");
        assert_eq!(
            json_of(&resumed),
            json_of(&latest),
            "a watermark-less checkpoint must fall back to a full fold"
        );

        // Same for a checkpoint claiming a position its log cannot account
        // for -- the log was truncated or replaced under it.
        let mut value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        value.as_object_mut().unwrap().insert(
            "checkpoint_seq".to_string(),
            serde_json::Value::from(9_999_u64),
        );
        std::fs::write(&path, serde_json::to_string(&value).unwrap()).expect("rewrite");
        let resumed = manager.load_conversation(&latest.id).expect("resume");
        assert_eq!(
            json_of(&resumed),
            json_of(&latest),
            "a checkpoint ahead of its log must fall back to a full fold"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_deleted_checkpoint_costs_nothing_but_time() {
        // The property the whole design is for: the checkpoint is a cache.
        let root = temp_root("ckpt_deleted");
        let (manager, latest) = checkpointed_then_advanced(&root);
        std::fs::remove_file(
            manager
                .conversations_dir()
                .join(format!("{}.json", latest.id)),
        )
        .expect("delete the checkpoint");

        let resumed = manager.load_conversation(&latest.id).expect("resume");
        assert_eq!(json_of(&resumed), json_of(&latest));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn continue_and_the_picker_see_a_session_with_no_checkpoint_yet() {
        // A short session checkpoints only on exit, so `--continue` and the
        // picker must find it from the log alone.
        let root = temp_root("no_ckpt_yet");
        let manager = ConversationManager::new(&root).expect("manager");
        let mut state = driven_state(&root);
        let snapshot = state.session.snapshot_conversation();
        let events = state.session.drain_events(&snapshot);
        manager
            .append_session_events(&snapshot, &events)
            .expect("append");
        assert!(
            !manager
                .conversations_dir()
                .join(format!("{}.json", snapshot.id))
                .exists(),
            "this test is about the no-checkpoint case"
        );

        let last = manager
            .load_last_conversation()
            .expect("continue")
            .expect("a session with a log is resumable");
        assert_eq!(last.id, snapshot.id);
        assert_eq!(json_of(&last), json_of(&snapshot));

        let metas = manager.list_conversation_metas().expect("picker list");
        assert_eq!(metas.len(), 1, "the picker must see it: {metas:?}");
        assert_eq!(metas[0].id, snapshot.id);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_legacy_session_without_a_log_still_loads() {
        // Written before the log existed: the snapshot is all there is, and
        // it stays the truth for that session.
        let root = temp_root("legacy");
        let manager = ConversationManager::new(&root).expect("manager");
        let mut state = driven_state(&root);
        let snapshot = state.session.snapshot_conversation();
        let _ = state.session.drain_events(&snapshot);
        manager.save_conversation(&snapshot).expect("save");
        std::fs::remove_file(
            manager
                .conversations_dir()
                .join(format!("{}.jsonl", snapshot.id)),
        )
        .ok();
        assert!(
            !manager
                .conversations_dir()
                .join(format!("{}.jsonl", snapshot.id))
                .exists()
        );

        let loaded = manager.load_conversation(&snapshot.id).expect("load");
        assert_eq!(json_of(&loaded), json_of(&snapshot));
        assert_eq!(
            manager
                .load_last_conversation()
                .expect("continue")
                .expect("legacy session is resumable")
                .id,
            snapshot.id
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_blocked_log_path_errors_instead_of_spinning() {
        // A directory where the log file goes. On Linux `File::open`
        // succeeds on a directory and every read then fails, which made
        // `BufRead::lines().count()` loop forever rather than error — CI hit
        // it as a 180s test timeout, not a failure. Both the append and the
        // fold must reach a verdict here.
        let root = temp_root("blocked_path");
        let manager = ConversationManager::new(&root).unwrap();
        let mut state = driven_state(&root);
        let snapshot = state.session.snapshot_conversation();
        let events = state.session.drain_events(&snapshot);
        std::fs::create_dir_all(
            manager
                .conversations_dir()
                .join(format!("{}.jsonl", snapshot.id)),
        )
        .expect("plant a directory in the log's place");

        assert!(
            manager.append_session_events(&snapshot, &events).is_err(),
            "append must fail on a blocked path"
        );
        assert!(
            manager
                .fold_conversation_from_log(&snapshot.id)
                .expect("fold must not error out")
                .is_none(),
            "a non-file path folds to nothing"
        );
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
