use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use mermaid_domain::ConversationHistory;
use mermaid_model::models::{ChatMessage, MessageRole};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

/// Reject a conversation id that doesn't match the generated shape
/// (`%Y%m%d_%H%M%S_%3f` => `YYYYMMDD_HHMMSS_mmm`). Without this, a
/// user-typed `/load <id>` (or `delete`) joins arbitrary text into a
/// filesystem path — `../../secret` would read/delete files outside the
/// project. Digits-and-underscores can't contain `/`, `\`, `..`, or a drive
/// prefix, so the format check alone closes the traversal.
fn validate_conversation_id(id: &str) -> Result<()> {
    let valid = id.len() == 19
        && id.as_bytes().iter().enumerate().all(|(i, b)| match i {
            8 | 15 => *b == b'_',
            _ => b.is_ascii_digit(),
        });
    anyhow::ensure!(valid, "invalid conversation id: {id:?}");
    Ok(())
}

/// Upper bound on a conversation file we'll read into memory (#129). A giant or
/// hostile `.mermaid/conversations/*.json` (or one with an enormous `content`)
/// would otherwise OOM the process — `--continue` walks every file. 64 MiB is
/// far above any real transcript yet bounds the worst case.
const MAX_CONVERSATION_BYTES: u64 = 64 * 1024 * 1024;

/// Read a conversation file with the [`MAX_CONVERSATION_BYTES`] cap enforced
/// *before* the bytes are pulled into RAM.
fn read_conversation_capped(path: &Path) -> std::io::Result<String> {
    let len = fs::metadata(path)?.len();
    if len > MAX_CONVERSATION_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "conversation file {} is {len} bytes, over the {} MiB cap",
                path.display(),
                MAX_CONVERSATION_BYTES / (1024 * 1024)
            ),
        ));
    }
    fs::read_to_string(path)
}

/// Marker left in a message's text when its screenshot bytes are dropped on save.
const SCREENSHOT_ELIDED_MARKER: &str = "\n[screenshot not persisted]";

/// Top-level key in a checkpoint file naming the last log `seq` folded into
/// it. Absent on legacy snapshots and on any file written before the log
/// existed, which is exactly the signal to fold from zero instead of
/// trusting the checkpoint. See `docs/design/fold-first-resume.md`.
const CHECKPOINT_SEQ_KEY: &str = "checkpoint_seq";

/// Return a sanitized copy of `messages` with computer-use screenshot bytes
/// removed before they reach durable storage (#99). Screenshots — which can
/// capture on-screen secrets — attach to **non-User** messages (the assistant
/// message the capture is routed onto, or a tool outcome); user-supplied
/// multimodal images attach to **User** messages and are intentional content,
/// so they're preserved. The live in-memory conversation is untouched (this
/// runs on a copy at the save chokepoint), so the chat and model context still
/// see the screenshot for the session — only the on-disk copy is scrubbed.
///
/// Returns `None` when nothing needed stripping, so the hot save path avoids a
/// clone in the common (no-screenshot) case.
fn strip_persisted_screenshots(messages: &[ChatMessage]) -> Option<Vec<ChatMessage>> {
    let needs = messages
        .iter()
        .any(|m| m.role != MessageRole::User && m.images.is_some());
    if !needs {
        return None;
    }
    let mut out = messages.to_vec();
    for m in out.iter_mut() {
        if m.role != MessageRole::User && m.images.is_some() {
            m.images = None;
            if !m.content.ends_with(SCREENSHOT_ELIDED_MARKER) {
                m.content.push_str(SCREENSHOT_ELIDED_MARKER);
            }
        }
    }
    Some(out)
}

/// Best-effort current git branch of `dir`, for labelling `--resume` rows.
/// `None` when `dir` isn't a git work tree, git is absent, or HEAD is
/// detached. Kept out of the pure reducer — callers invoke it in the impure
/// startup path and stamp the result onto the conversation.
#[must_use]
pub fn detect_git_branch(dir: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    // A detached HEAD reports the literal "HEAD"; treat that as no branch.
    (!branch.is_empty() && branch != "HEAD").then_some(branch)
}

/// Best-effort short git SHA of `dir`'s HEAD, for session provenance. `None`
/// outside a git work tree or when git is absent. Impure — stamped at startup
/// alongside `detect_git_branch`, never in the reducer.
#[must_use]
pub fn detect_git_sha(dir: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!sha.is_empty()).then_some(sha)
}

/// Lightweight session metadata, persisted as an `<id>.meta` sidecar so listing
/// sessions doesn't have to read a transcript at all.
///
/// A cache of a cache: the log is the truth, the checkpoint materializes it,
/// and this indexes the checkpoint. Written on every append, because with
/// checkpoints on a coarse cadence a short session has no checkpoint yet and
/// would otherwise be invisible to the picker. A session missing one is
/// listed by reading its checkpoint, or failing that by folding its log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationMeta {
    pub id: String,
    pub title: String,
    pub updated_at: DateTime<Local>,
    #[serde(default)]
    pub git_branch: Option<String>,
    #[serde(default)]
    pub message_count: usize,
    #[serde(default)]
    pub forked_from: Option<String>,
}

impl ConversationMeta {
    fn from_history(h: &ConversationHistory) -> Self {
        Self {
            id: h.id.clone(),
            title: h.title.clone(),
            updated_at: h.updated_at,
            git_branch: h.git_branch.clone(),
            message_count: h.messages().len(),
            forked_from: h.forked_from.clone(),
        }
    }
}

/// Read a checkpoint file: the conversation plus the log watermark it was
/// materialized at, when it carries one.
///
/// The watermark is read off the raw JSON rather than the deserialized
/// value, because it is deliberately not a field of `ConversationHistory`
/// (see [`CHECKPOINT_SEQ_KEY`]). A file without it is a legacy snapshot or
/// one written by a build that did not stamp, and the caller treats a
/// missing watermark as "cannot trust this as a checkpoint".
fn read_checkpoint(path: &Path) -> Result<(ConversationHistory, Option<u64>)> {
    let json = read_conversation_capped(path)?;
    let value: serde_json::Value = serde_json::from_str(&json)?;
    let seq = value
        .get(CHECKPOINT_SEQ_KEY)
        .and_then(serde_json::Value::as_u64);
    let conversation: ConversationHistory = serde_json::from_value(value)?;
    Ok((conversation, seq))
}

/// Manages conversation persistence for a project
#[derive(Clone)]
pub struct ConversationManager {
    /// The project directory the manager was built for — keys the
    /// scratchpad cascade in [`ConversationManager::delete_conversation`].
    project_dir: PathBuf,
    conversations_dir: PathBuf,
    /// The per-session `.jsonl` appender/reader (see `event_log`). Shared
    /// across clones like `seen`, so one process keeps one seq cursor.
    events: Arc<crate::session::event_log::EventLog>,
}

impl ConversationManager {
    /// Create a new conversation manager for a project directory
    ///
    /// # Errors
    ///
    /// Creating `.mermaid/conversations` under `project_dir` — a read-only
    /// or unwritable project. It is created here, so the later load and list
    /// paths can treat an unreadable directory as "no conversations" rather
    /// than a failure.
    pub fn new(project_dir: impl AsRef<Path>) -> Result<Self> {
        let conversations_dir = project_dir.as_ref().join(".mermaid").join("conversations");
        fs::create_dir_all(&conversations_dir)?;

        Ok(Self {
            project_dir: project_dir.as_ref().to_path_buf(),
            events: Arc::new(crate::session::event_log::EventLog::new(
                conversations_dir.clone(),
            )),
            conversations_dir,
        })
    }

    /// Append session events for `snapshot` to its `.jsonl` log, creating
    /// the log (with a backfill from the snapshot) on first touch. Called by
    /// the persistence chain BEFORE the snapshot rewrite, so the history
    /// lands before the file it explains is overwritten.
    ///
    /// # Errors
    ///
    /// An `id` that would escape the conversations dir, and the append I/O
    /// itself. The caller treats a failure as a warning: the snapshot save
    /// must still run, and the log self-heals on the next save.
    pub fn append_session_events(
        &self,
        snapshot: &ConversationHistory,
        events: &[mermaid_domain::SessionEvent],
    ) -> Result<()> {
        validate_conversation_id(&snapshot.id)?;
        let appended = self.events.append(snapshot, events);
        // The picker's index rides the append, not the checkpoint. With
        // checkpoints on a ~200-event cadence a short session has none at
        // all until it exits, so a sidecar written only alongside one would
        // leave real sessions invisible to `--resume`. Best-effort as ever:
        // the sidecar is a cache of a cache.
        self.write_meta(snapshot);
        appended
    }

    /// Write the tiny `<id>.meta` sidecar the session picker lists from.
    /// Best-effort by construction — every field is recoverable from the
    /// log, so a failed write costs a slower listing, never data.
    fn write_meta(&self, conversation: &ConversationHistory) {
        if conversation.messages().is_empty() {
            return;
        }
        let meta = ConversationMeta::from_history(conversation);
        if let Ok(json) = serde_json::to_string(&meta) {
            let path = self
                .conversations_dir
                .join(format!("{}.meta", conversation.id));
            let _ = mermaid_runtime::write_atomic_with_mode(&path, json.as_bytes(), 0o600);
        }
    }

    /// Where a session's event log lives. The compaction bookkeeping row
    /// records this instead of a per-compaction archive file: the dropped
    /// messages are the log's earlier `message` events.
    #[must_use]
    pub fn event_log_path(&self, id: &str) -> PathBuf {
        self.events.path_for(id)
    }

    /// Read a session's log as EVENTS rather than folding it into a
    /// conversation — for readers that want the history in the order it
    /// happened instead of the state it produced.
    ///
    /// The daemon's `subscribe_task` catch-up is that reader: a mid-run
    /// attach replays these onto the `RunEvent` wire so a subscriber joining
    /// at minute nine learns what the first nine produced. `Ok(None)` when
    /// there is no readable log; a torn tail yields the prefix, exactly as a
    /// fold does.
    ///
    /// # Errors
    ///
    /// An `id` that would escape the conversations dir, and read I/O on the
    /// log file. A log that is absent, over the cap, or newer-format is
    /// `Ok(None)`, not an error.
    pub fn read_session_events(
        &self,
        id: &str,
    ) -> Result<Option<Vec<mermaid_domain::SessionEvent>>> {
        validate_conversation_id(id)?;
        Ok(self
            .events
            .read_events(id, None)?
            .map(|(events, _highest)| events))
    }

    /// Rebuild a conversation from its event log — the recovery source when
    /// the snapshot is missing or will not parse. `Ok(None)` when there is
    /// no (foldable) log.
    ///
    /// # Errors
    ///
    /// An `id` that would escape the conversations dir, and read I/O on the
    /// log file. A log that is absent, capped, newer-format, or headerless
    /// is `Ok(None)`, not an error.
    pub fn fold_conversation_from_log(&self, id: &str) -> Result<Option<ConversationHistory>> {
        validate_conversation_id(id)?;
        let Some(folded) = self.events.fold(id)? else {
            return Ok(None);
        };
        // The folded id drives later saves exactly like a parsed one; hold
        // it to the same rule.
        validate_conversation_id(&folded.id)?;
        Ok(Some(folded))
    }

    /// Save a conversation to disk
    ///
    /// # Errors
    ///
    /// An `id` that would escape the conversations dir, serializing the
    /// conversation, and the atomic 0600 write. Two cases that look like
    /// failures are `Ok`: a message-less conversation is deliberately not
    /// persisted, and a concurrent writer detected through the `(mtime, len)`
    /// baseline diverts this copy to a `.conflict` sibling and warns instead
    /// of overwriting. The `.meta` sidecar is best-effort and never fails the
    /// save.
    pub fn save_conversation(&self, conversation: &ConversationHistory) -> Result<()> {
        // The id field is persisted and round-trips through (potentially
        // tampered) on-disk state; validate it before it drives the write path,
        // so a loaded conversation can't escape the conversations dir on save.
        validate_conversation_id(&conversation.id)?;

        // An untouched session — the user ran `mermaid` and closed it without
        // sending anything — has no transcript. Never persist it, so it can't
        // clutter the `--resume` picker or be reached by `--continue`; the first
        // real message triggers the next save, which creates the file then.
        if conversation.messages().is_empty() {
            return Ok(());
        }

        let filename = format!("{}.json", conversation.id);
        let path = self.conversations_dir.join(filename);

        // Sanitize before persisting: strip computer-use screenshot bytes (#99)
        // AND scrub credential-shaped strings, so a persisted `read_file` of
        // `.env` or an API error echoing a key can't sit in cleartext (mirrors
        // the --record redaction in recorder.rs). Only clones when scrubbing.
        let mut value = match strip_persisted_screenshots(conversation.messages()) {
            Some(sanitized) => {
                let mut stripped = conversation.clone();
                *stripped.messages_mut() = sanitized;
                serde_json::to_value(&stripped)?
            },
            None => serde_json::to_value(conversation)?,
        };
        mermaid_model::utils::redact_json(&mut value);
        // Stamp WHICH events this checkpoint already contains. Storage's
        // business, not the domain's: a `ConversationHistory` describes a
        // conversation, while the log offset a cache was materialized at
        // describes the cache. Injected into the serialized object rather
        // than added as a field, and unknown keys are ignored on the way
        // back in, so the value round-trips through the reducer untouched
        // and an older mermaid reads the file as a plain snapshot.
        if let Some(seq) = self.events.checkpoint_seq(&conversation.id)
            && let Some(object) = value.as_object_mut()
        {
            object.insert(CHECKPOINT_SEQ_KEY.to_string(), seq.into());
        }
        let json = serde_json::to_string_pretty(&value)?;

        // F73's concurrent-writer guard is NOT here any more; it moved to the
        // append (see `event_log::diverted_on_conflict`). Two reasons, both
        // consequences of the log becoming the truth: this file is now a
        // derived cache, so a clobbered checkpoint costs a longer replay
        // rather than lost history — and by the time a save reaches here the
        // append has already decided whether this process is still writing
        // the shared session at all. Guarding the cache after the truth was
        // written would only produce `.conflict` copies of a rebuildable file.

        // Atomic write: a crash mid-save must not leave a half-written
        // checkpoint that resume would then have to distrust.
        // Owner-only (0o600): the transcript can carry secrets in cleartext.
        mermaid_runtime::write_atomic_with_mode(&path, json.as_bytes(), 0o600)?;
        // Refresh our baseline to the file we just wrote so the NEXT save by this
        // process compares against our own write, not the pre-save state.

        // Keep the picker's sidecar current for a checkpoint written without
        // a preceding append (the QA paths do this). The append writes it
        // too, which is what covers sessions with no checkpoint yet.
        self.write_meta(conversation);

        Ok(())
    }

    /// Load a specific conversation by ID
    ///
    /// # Errors
    ///
    /// An `id` that would escape the conversations dir, a file that is
    /// missing, unreadable, or past the size cap, JSON that does not parse,
    /// and a parsed `id` that would itself traverse — checked separately,
    /// because that field is on-disk state that drives later saves.
    pub fn load_conversation(&self, id: &str) -> Result<ConversationHistory> {
        validate_conversation_id(id)?;
        let path = self.conversations_dir.join(format!("{id}.json"));
        let checkpoint = read_checkpoint(&path);

        // A session with no log is one that predates it: the snapshot is
        // all there is, so it is still the truth for that session. Every
        // other case goes through the log.
        if !self.events.exists(id) {
            let (conversation, _) = checkpoint?;
            validate_conversation_id(&conversation.id)?;
            return Ok(conversation);
        }

        // Checkpoint plus the events after its watermark. Anything that
        // makes the checkpoint untrustworthy — unreadable, unparseable, no
        // watermark, or a watermark its log cannot account for — falls
        // through to folding the log from zero. That fallback is the
        // property this design is for: the checkpoint is never
        // load-bearing, only faster.
        if let Ok((checkpoint, Some(seq))) = checkpoint
            && validate_conversation_id(&checkpoint.id).is_ok()
            && let Some(resumed) = self.events.replay_onto(id, checkpoint, seq)?
        {
            return Ok(resumed);
        }

        let folded = self
            .fold_conversation_from_log(id)?
            .with_context(|| format!("session {id} has a log that could not be folded"))?;
        Ok(folded)
    }

    /// Load the most recent *valid* conversation.
    ///
    /// Iterates files newest-first by mtime and returns the first that reads,
    /// parses, and has a valid id — skipping (with a warning) any unreadable,
    /// unparseable, or traversing-id file. Mirrors `list_conversations`'s
    /// tolerance so one corrupt/partial file (e.g. a crash mid-write) can't make
    /// `--continue` hard-fail; it falls back to the next-newest valid conversation.
    ///
    /// # Errors
    ///
    /// In practice none: an unreadable conversations dir and every unreadable,
    /// oversized, unparseable, or traversing-id file are skipped with a
    /// warning, and running out of candidates is `Ok(None)`. The `Result` is
    /// kept for callers that already handle one and so this can grow a real
    /// failure later.
    pub fn load_last_conversation(&self) -> Result<Option<ConversationHistory>> {
        // Newest-first over LOGS, then checkpoints for sessions that predate
        // the log. Ranking by log mtime matters: a session's checkpoint can
        // be many events stale (it is written every ~200), so ordering by
        // checkpoint mtime would answer with the wrong session.
        for id in self.session_ids_newest_first() {
            // One resume algorithm, whatever the entry point: `load_conversation`
            // already picks checkpoint-plus-replay or a full fold.
            let conv = match self.load_conversation(&id) {
                Ok(conv) => conv,
                Err(error) => {
                    tracing::warn!(id, %error, "skipping session that would not load");
                    continue;
                },
            };
            // Skip untouched (message-less) sessions — `--continue` resumes the
            // last chat with real history, not a blank one opened and closed.
            if conv.messages().is_empty() {
                continue;
            }
            return Ok(Some(conv));
        }
        Ok(None)
    }

    /// Every session id in this project, newest activity first.
    ///
    /// Ranked by the LOG's mtime where there is one, since that is what a
    /// save touches every time; a checkpoint is written on a much coarser
    /// cadence and would rank a busy session as stale. Sessions with only a
    /// checkpoint (written before logs existed) rank by that instead.
    fn session_ids_newest_first(&self) -> Vec<String> {
        let Ok(entries) = fs::read_dir(&self.conversations_dir) else {
            return Vec::new();
        };
        // Per id, keep the log's mtime if there is a log, else the
        // checkpoint's. `is_log` wins over recency, not the other way
        // round: a checkpoint written after the last append is still the
        // coarser clock.
        let mut best: HashMap<String, (bool, SystemTime)> = HashMap::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(ext) = path.extension().and_then(|ext| ext.to_str()) else {
                continue;
            };
            let is_log = match ext {
                "jsonl" => true,
                "json" => false,
                _ => continue,
            };
            let Some(id) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            if validate_conversation_id(id).is_err() {
                continue;
            }
            let Ok(mtime) = entry.metadata().and_then(|meta| meta.modified()) else {
                continue;
            };
            match best.get(id) {
                Some((had_log, _)) if *had_log && !is_log => {},
                _ => {
                    best.insert(id.to_string(), (is_log, mtime));
                },
            }
        }
        let mut ranked: Vec<(SystemTime, String)> = best
            .into_iter()
            .map(|(id, (_, mtime))| (mtime, id))
            .collect();
        ranked.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
        ranked.into_iter().map(|(_, id)| id).collect()
    }

    /// List all conversations in the project
    ///
    /// # Errors
    ///
    /// In practice none, and deliberately: an unreadable directory yields an
    /// empty list, and any file that will not read or parse is skipped, so one
    /// corrupt transcript cannot empty the picker.
    pub fn list_conversations(&self) -> Result<Vec<ConversationHistory>> {
        let mut conversations = Vec::new();

        // Read all JSON files in the conversations directory
        if let Ok(entries) = fs::read_dir(&self.conversations_dir) {
            for entry in entries.flatten() {
                if let Some(ext) = entry.path().extension()
                    && ext == "json"
                    && let Ok(json) = read_conversation_capped(&entry.path())
                    && let Ok(conv) = serde_json::from_str::<ConversationHistory>(&json)
                    // Skip untouched (message-less) sessions — they carry no
                    // history worth resuming, so they never appear in the picker.
                    && !conv.messages().is_empty()
                {
                    conversations.push(conv);
                }
            }
        }

        // Sort by updated_at (newest first)
        conversations.sort_by_key(|c| std::cmp::Reverse(c.updated_at));

        Ok(conversations)
    }

    /// Fast session list: read each `<id>.meta` sidecar; for a session that
    /// lacks a (valid) one — older, or written by a pre-sidecar build — fall
    /// back to fully parsing its `<id>.json`. Message-less sessions are skipped.
    /// Newest-first. Cheaper than [`Self::list_conversations`] for display-only paths.
    ///
    /// # Errors
    ///
    /// In practice none, matching [`Self::list_conversations`]: an unreadable
    /// directory yields an empty list, and a sidecar that will not read or
    /// parse falls through to parsing its `<id>.json`, which is itself skipped
    /// if that fails too.
    pub fn list_conversation_metas(&self) -> Result<Vec<ConversationMeta>> {
        let mut metas = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let Ok(entries) = fs::read_dir(&self.conversations_dir) else {
            return Ok(metas);
        };
        let paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        // Fast path: `<id>.meta` sidecars.
        for path in &paths {
            if path.extension().is_some_and(|e| e == "meta")
                && let Ok(raw) = fs::read_to_string(path)
                && let Ok(meta) = serde_json::from_str::<ConversationMeta>(&raw)
                && meta.message_count > 0
            {
                seen.insert(meta.id.clone());
                metas.push(meta);
            }
        }
        // Fallback: any `<id>.json` without a valid sidecar.
        for path in &paths {
            if path.extension().is_some_and(|e| e == "json")
                && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
                && !seen.contains(stem)
                && let Ok(json) = read_conversation_capped(path)
                && let Ok(conv) = serde_json::from_str::<ConversationHistory>(&json)
                && !conv.messages().is_empty()
            {
                seen.insert(stem.to_string());
                metas.push(ConversationMeta::from_history(&conv));
            }
        }
        // Last resort: a session with a log but neither sidecar nor
        // checkpoint. Rare (both are written on every save), but the log is
        // the truth, so a session that has one must be listable — otherwise
        // the picker would hide a resumable session because a cache is
        // missing. Folding here is the expensive path, which is exactly why
        // it runs only for what the two cheap passes missed.
        for path in &paths {
            if path.extension().is_some_and(|e| e == "jsonl")
                && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
                && !seen.contains(stem)
                && let Ok(Some(conv)) = self.fold_conversation_from_log(stem)
                && !conv.messages().is_empty()
            {
                metas.push(ConversationMeta::from_history(&conv));
            }
        }
        metas.sort_by_key(|m| std::cmp::Reverse(m.updated_at));
        Ok(metas)
    }

    /// Delete a conversation (and its metadata sidecar).
    ///
    /// # Errors
    ///
    /// An `id` that would escape the conversations dir, and removing the
    /// `<id>.json` itself. An id with no file is `Ok`, and the `.meta` sidecar
    /// and the session scratch dir are cleaned up best-effort — neither can
    /// fail the delete.
    pub fn delete_conversation(&self, id: &str) -> Result<()> {
        validate_conversation_id(id)?;
        // The index row and what hangs off it in the runtime store. Best-effort
        // like the sidecars: the files are the truth, and the daemon's next
        // start prunes any row this misses.
        let _ = mermaid_runtime::with_shared_store(|store| store.sessions().delete(id));
        let path = self.conversations_dir.join(format!("{id}.json"));
        if path.exists() {
            fs::remove_file(path)?;
        }
        // Best-effort sidecar + event-log cleanup — their absence is harmless.
        let _ = fs::remove_file(self.conversations_dir.join(format!("{id}.meta")));
        let _ = fs::remove_file(self.conversations_dir.join(format!("{id}.jsonl")));
        // Cascade to the session's scratch directory (skipped if another
        // live mermaid still holds its pid lock). Best-effort: the sweep
        // eventually reaps whatever this misses.
        let _ = crate::session::scratchpad::remove(&self.project_dir, id);

        Ok(())
    }

    /// Get the conversations directory path
    #[must_use]
    pub fn conversations_dir(&self) -> &Path {
        &self.conversations_dir
    }
}

/// Probe the session's provenance. Impure — spawns `git` twice — so it is a
/// value the shell resolves once at startup and delivers as
/// `Msg::SessionProvenanceResolved`.
#[must_use]
pub fn probe_session_provenance(cwd: &Path) -> mermaid_domain::SessionProvenance {
    mermaid_domain::SessionProvenance {
        git_branch: detect_git_branch(cwd),
        git_sha: detect_git_sha(cwd),
        cli_version: Some(env!("CARGO_PKG_VERSION").to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A conversation carrying one message, so it actually persists — empty
    /// (message-less) sessions are intentionally not saved.
    fn touched(project: &str) -> ConversationHistory {
        let mut c = ConversationHistory::new(project.into(), "m".into(), Local::now());
        c.add_messages(&[ChatMessage::user("hi")], Local::now());
        c
    }

    #[test]
    fn legacy_conversation_json_without_git_branch_deserializes() {
        // Every session saved before the `--resume` picker existed lacks a
        // `git_branch` key; `#[serde(default)]` must load it as `None` rather
        // than failing the picker's `list_conversations`.
        let json = r#"{
            "id": "20260101_120000_001",
            "title": "Legacy session",
            "messages": [],
            "model_name": "ollama/test",
            "project_path": "/tmp/proj",
            "created_at": "2026-01-01T12:00:00-05:00",
            "updated_at": "2026-01-01T12:00:00-05:00",
            "total_tokens": null
        }"#;
        let conv: ConversationHistory = serde_json::from_str(json).expect("legacy json loads");
        assert!(conv.git_branch.is_none());
        assert_eq!(conv.title, "Legacy session");
        // And a round-trip of a branch-bearing conversation preserves it.
        let mut fresh =
            ConversationHistory::new("/tmp/proj".to_string(), "m".to_string(), Local::now());
        fresh.git_branch = Some("feature/x".to_string());
        let round: ConversationHistory =
            serde_json::from_str(&serde_json::to_string(&fresh).unwrap()).unwrap();
        assert_eq!(round.git_branch.as_deref(), Some("feature/x"));
    }

    #[test]
    fn legacy_json_defaults_session_state_fields() {
        // A file written before the session-state fields existed lacks them;
        // `#[serde(default)]` must load safety/meters as None/0 (safety then
        // falls back to the config default on resume — see `seed_conversation`).
        let json = r#"{
            "id": "20260101_120000_002",
            "title": "Old",
            "messages": [],
            "model_name": "m",
            "project_path": "/tmp/proj",
            "created_at": "2026-01-01T12:00:00-05:00",
            "updated_at": "2026-01-01T12:00:00-05:00",
            "total_tokens": null
        }"#;
        let conv: ConversationHistory = serde_json::from_str(json).expect("legacy json loads");
        assert_eq!(conv.safety_mode, None);
        assert_eq!(
            conv.cumulative_token_usage,
            mermaid_domain::TokenUsageTotals::default()
        );
        assert!(conv.last_token_usage.is_none());
        assert!(conv.context_usage.is_none());
        assert!(conv.tasks.tasks.is_empty());
        assert_eq!(conv.tasks.next_id, 0);
        assert!(
            conv.advertised_context.is_none(),
            "pre-field saves load a None baseline (silent seed)"
        );
    }

    #[test]
    fn advertised_context_round_trips_through_conversation_json() {
        let mut fresh = touched("/tmp/proj");
        fresh.advertised_context = Some(mermaid_domain::AdvertisedContext {
            plan_path: Some(std::path::PathBuf::from("/tmp/proj/.mermaid/plans/x.md")),
            safety_mode: mermaid_runtime::SafetyMode::Ask,
            model_id: "ollama/test".to_string(),
        });
        let round: ConversationHistory =
            serde_json::from_str(&serde_json::to_string(&fresh).unwrap()).unwrap();
        let ctx = round.advertised_context.expect("field survives");
        assert_eq!(
            ctx.plan_path.as_deref(),
            Some(std::path::Path::new("/tmp/proj/.mermaid/plans/x.md"))
        );
        assert_eq!(ctx.model_id, "ollama/test");
    }

    #[test]
    fn tasks_round_trip_through_conversation_json() {
        let mut fresh = touched("/tmp/proj");
        fresh.tasks.create(
            vec![mermaid_domain::ChecklistSpec {
                subject: "wire broker".into(),
                active_form: "wiring broker".into(),
                description: Some("through ExecContext".into()),
                in_progress: true,
            }],
            mermaid_domain::ChecklistOrigin::Model,
            mermaid_domain::Stamp {
                now_epoch: 42,
                run_tokens: 7,
            },
        );
        let round: ConversationHistory =
            serde_json::from_str(&serde_json::to_string(&fresh).unwrap()).unwrap();
        assert_eq!(round.tasks, fresh.tasks);
        assert_eq!(round.tasks.tasks[0].started_at, Some(42));
    }

    #[test]
    fn session_state_round_trips_through_json() {
        let mut conv = ConversationHistory::new("/tmp/p".into(), "m".into(), Local::now());
        conv.safety_mode = Some(mermaid_runtime::SafetyMode::FullAccess);
        conv.cumulative_token_usage = mermaid_domain::TokenUsageTotals {
            prompt_tokens: 777,
            ..Default::default()
        };
        let round: ConversationHistory =
            serde_json::from_str(&serde_json::to_string(&conv).unwrap()).unwrap();
        assert_eq!(
            round.safety_mode,
            Some(mermaid_runtime::SafetyMode::FullAccess)
        );
        assert_eq!(round.cumulative_token_usage.total_tokens(), 777);
    }

    #[test]
    fn validate_conversation_id_rejects_traversal() {
        assert!(validate_conversation_id("20260101_120000_001").is_ok());
        assert!(validate_conversation_id("../secret").is_err());
        assert!(validate_conversation_id("..\\secret").is_err());
        assert!(validate_conversation_id("/etc/passwd").is_err());
        assert!(validate_conversation_id("20260101_120000").is_err()); // too short
        assert!(validate_conversation_id("abcdefgh_120000_001").is_err()); // non-digits
    }

    #[test]
    fn strip_persisted_screenshots_drops_assistant_images_keeps_user_images() {
        let messages = vec![
            ChatMessage::user("look at this").with_images(vec!["USER_PASTED_B64".to_string()]),
            ChatMessage::assistant("here is the screen")
                .with_images(vec!["SCREENSHOT_B64".to_string()]),
            ChatMessage::assistant("no image here"),
        ];
        let sanitized = strip_persisted_screenshots(&messages).expect("had a screenshot to strip");
        // User-supplied image preserved.
        assert_eq!(
            sanitized[0].images.as_deref(),
            Some(["USER_PASTED_B64".to_string()].as_slice())
        );
        // Assistant screenshot dropped + marker added.
        assert!(sanitized[1].images.is_none());
        assert!(sanitized[1].content.ends_with(SCREENSHOT_ELIDED_MARKER));
        // Untouched assistant message is unchanged (no spurious marker).
        assert!(!sanitized[2].content.ends_with(SCREENSHOT_ELIDED_MARKER));
    }

    #[test]
    fn strip_persisted_screenshots_is_none_without_assistant_images() {
        let messages = vec![
            ChatMessage::user("hi").with_images(vec!["USER_B64".to_string()]),
            ChatMessage::assistant("no images"),
        ];
        assert!(strip_persisted_screenshots(&messages).is_none());
    }

    #[test]
    fn saved_conversation_json_has_no_screenshot_bytes() {
        let dir = std::env::temp_dir().join("mermaid_strip_test");
        let _ = fs::create_dir_all(&dir);
        let mut conv = ConversationHistory::new("/tmp/p".into(), "m".into(), Local::now());
        *conv.messages_mut() = vec![
            ChatMessage::user("u").with_images(vec!["USERIMG".to_string()]),
            ChatMessage::assistant("a").with_images(vec!["SHOTBYTES".to_string()]),
        ];
        let store = ConversationManager {
            project_dir: dir.clone(),
            events: Arc::new(crate::session::event_log::EventLog::new(dir.clone())),
            conversations_dir: dir.clone(),
        };
        store.save_conversation(&conv).expect("save");
        let raw = fs::read_to_string(dir.join(format!("{}.json", conv.id))).expect("read");
        assert!(
            !raw.contains("SHOTBYTES"),
            "screenshot leaked to disk: {raw}"
        );
        assert!(raw.contains("USERIMG"), "user image should persist");
        // Live conversation untouched — still carries the screenshot in-session.
        assert_eq!(
            conv.messages()[1].images.as_deref(),
            Some(["SHOTBYTES".to_string()].as_slice())
        );
        let _ = fs::remove_file(dir.join(format!("{}.json", conv.id)));
    }

    #[test]
    fn saved_conversation_redacts_secrets_and_is_owner_only() {
        let dir = std::env::temp_dir().join(format!("mermaid_conv_redact_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
        let mut conv = ConversationHistory::new("/tmp/p".into(), "m".into(), Local::now());
        // A read_file of .env lands in a tool-result message in cleartext today.
        *conv.messages_mut() = vec![
            ChatMessage::user("read .env"),
            ChatMessage::assistant("OPENAI_API_KEY=sk-abcdefghijklmnop1234"),
        ];
        let store = ConversationManager {
            project_dir: dir.clone(),
            events: Arc::new(crate::session::event_log::EventLog::new(dir.clone())),
            conversations_dir: dir.clone(),
        };
        store.save_conversation(&conv).expect("save");
        let path = dir.join(format!("{}.json", conv.id));
        let raw = fs::read_to_string(&path).expect("read");
        assert!(
            !raw.contains("sk-abcdefghijklmnop1234"),
            "secret leaked to the conversation store: {raw}"
        );
        assert!(
            raw.contains("[REDACTED]"),
            "expected redaction marker: {raw}"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                mode, 0o600,
                "conversation file must be owner-only, got {mode:o}"
            );
        }
        // Live conversation untouched — the model still sees the real content in-session.
        assert!(
            conv.messages()[1]
                .content
                .contains("sk-abcdefghijklmnop1234")
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_new_conversation_has_session_title() {
        let conv =
            ConversationHistory::new("/tmp/project".into(), "test-model".into(), Local::now());
        assert!(conv.title.starts_with("Session "));
        assert_eq!(conv.model_name, "test-model");
        assert_eq!(conv.project_path, "/tmp/project");
        assert!(conv.messages().is_empty());
    }

    #[test]
    fn test_title_updates_from_first_user_message() {
        let mut conv = ConversationHistory::new("/tmp".into(), "m".into(), Local::now());
        conv.add_messages(&[ChatMessage::user("Fix the login bug")], Local::now());
        assert_eq!(conv.title, "Fix the login bug");
    }

    #[test]
    fn test_title_truncated_at_60_chars() {
        let mut conv = ConversationHistory::new("/tmp".into(), "m".into(), Local::now());
        let long_msg = "a".repeat(100);
        conv.add_messages(&[ChatMessage::user(long_msg)], Local::now());
        assert!(conv.title.ends_with("..."));
        assert!(conv.title.len() <= 64); // 60 chars + "..."
    }

    #[test]
    fn test_title_set_only_once() {
        let mut conv = ConversationHistory::new("/tmp".into(), "m".into(), Local::now());
        conv.add_messages(&[ChatMessage::user("First message")], Local::now());
        conv.add_messages(&[ChatMessage::user("Second message")], Local::now());
        assert_eq!(conv.title, "First message");
    }

    #[test]
    fn test_input_history_deduplication() {
        let mut conv = ConversationHistory::new("/tmp".into(), "m".into(), Local::now());
        conv.add_to_input_history("hello".into());
        conv.add_to_input_history("hello".into()); // duplicate
        conv.add_to_input_history("world".into());
        assert_eq!(conv.input_history.len(), 2);
    }

    #[test]
    fn test_input_history_skips_empty() {
        let mut conv = ConversationHistory::new("/tmp".into(), "m".into(), Local::now());
        conv.add_to_input_history("".into());
        conv.add_to_input_history("   ".into());
        assert_eq!(conv.input_history.len(), 0);
    }

    #[test]
    fn test_input_history_capped_at_100() {
        let mut conv = ConversationHistory::new("/tmp".into(), "m".into(), Local::now());
        for i in 0..110 {
            conv.add_to_input_history(format!("msg{i}"));
        }
        assert_eq!(conv.input_history.len(), 100);
        assert_eq!(conv.input_history.front().unwrap(), "msg10");
    }

    #[test]
    fn sidecar_powers_metadata_listing() {
        let dir = std::env::temp_dir().join("mermaid_test_meta_sidecar");
        let _ = fs::remove_dir_all(&dir);
        let manager = ConversationManager::new(&dir).unwrap();
        let mut conv = ConversationHistory::new("/tmp/proj".into(), "model".into(), Local::now());
        conv.title = "My session".into();
        conv.add_messages(
            &[ChatMessage::user("hi"), ChatMessage::user("there")],
            Local::now(),
        );
        manager.save_conversation(&conv).unwrap();

        assert!(
            manager
                .conversations_dir()
                .join(format!("{}.meta", conv.id))
                .exists()
        );
        let metas = manager.list_conversation_metas().unwrap();
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].id, conv.id);
        assert_eq!(metas[0].title, "My session");
        assert_eq!(metas[0].message_count, 2);

        // Deleting the session removes its sidecar too.
        manager.delete_conversation(&conv.id).unwrap();
        assert!(
            !manager
                .conversations_dir()
                .join(format!("{}.meta", conv.id))
                .exists()
        );
        assert!(manager.list_conversation_metas().unwrap().is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn metadata_listing_falls_back_to_full_parse_without_sidecar() {
        let dir = std::env::temp_dir().join("mermaid_test_meta_fallback");
        let _ = fs::remove_dir_all(&dir);
        let manager = ConversationManager::new(&dir).unwrap();
        let mut conv = ConversationHistory::new("/tmp".into(), "model".into(), Local::now());
        conv.add_messages(&[ChatMessage::user("hi")], Local::now());
        manager.save_conversation(&conv).unwrap();
        // Simulate a pre-sidecar session by removing the sidecar.
        fs::remove_file(
            manager
                .conversations_dir()
                .join(format!("{}.meta", conv.id)),
        )
        .unwrap();
        let metas = manager.list_conversation_metas().unwrap();
        assert_eq!(metas.len(), 1, "falls back to parsing the .json");
        assert_eq!(metas[0].message_count, 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn lineage_fields_default_on_old_sessions() {
        // A transcript persisted before the lineage fields existed still loads.
        let json = r#"{"id":"x","title":"t","messages":[],"model_name":"m","project_path":"/p","created_at":"2026-01-01T00:00:00+00:00","updated_at":"2026-01-01T00:00:00+00:00","total_tokens":null}"#;
        let conv: ConversationHistory = serde_json::from_str(json).unwrap();
        assert!(conv.git_sha.is_none());
        assert!(conv.cli_version.is_none());
        assert!(conv.forked_from.is_none());
        assert!(conv.parent_session.is_none());
    }

    #[test]
    fn test_save_load_roundtrip() {
        let dir = std::env::temp_dir().join("mermaid_test_conv_roundtrip");
        let _ = fs::remove_dir_all(&dir);
        let manager = ConversationManager::new(&dir).unwrap();

        let mut conv = ConversationHistory::new("/tmp".into(), "model".into(), Local::now());
        conv.add_messages(&[ChatMessage::user("test message")], Local::now());
        conv.add_to_input_history("test message".into());

        manager.save_conversation(&conv).unwrap();
        let loaded = manager.load_conversation(&conv.id).unwrap();

        assert_eq!(loaded.id, conv.id);
        assert_eq!(loaded.title, conv.title);
        assert_eq!(loaded.messages().len(), 1);
        assert_eq!(loaded.input_history.len(), 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_list_conversations_ordered_by_updated_at() {
        let dir = std::env::temp_dir().join("mermaid_test_conv_list");
        let _ = fs::remove_dir_all(&dir);
        let manager = ConversationManager::new(&dir).unwrap();

        let conv1 = touched("/tmp");
        std::thread::sleep(std::time::Duration::from_millis(10));
        let conv2 = touched("/tmp");

        manager.save_conversation(&conv1).unwrap();
        manager.save_conversation(&conv2).unwrap();

        let list = manager.list_conversations().unwrap();
        assert_eq!(list.len(), 2);
        // Newest first
        assert_eq!(list[0].id, conv2.id);
        assert_eq!(list[1].id, conv1.id);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_last_conversation() {
        let dir = std::env::temp_dir().join("mermaid_test_conv_last");
        let _ = fs::remove_dir_all(&dir);
        let manager = ConversationManager::new(&dir).unwrap();

        assert!(manager.load_last_conversation().unwrap().is_none());

        let conv = touched("/tmp");
        manager.save_conversation(&conv).unwrap();

        let last = manager.load_last_conversation().unwrap().unwrap();
        assert_eq!(last.id, conv.id);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_last_conversation_picks_newest_by_mtime() {
        // Writes three conversations with staggered mtimes (via sleeps
        // between saves) and asserts the mtime-based picker returns the
        // last one written — even though filename-alphabetical ordering
        // would pick a different file.
        let dir = std::env::temp_dir().join("mermaid_test_conv_mtime");
        let _ = fs::remove_dir_all(&dir);
        let manager = ConversationManager::new(&dir).unwrap();

        let conv1 = touched("/tmp");
        manager.save_conversation(&conv1).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));

        let conv2 = touched("/tmp");
        manager.save_conversation(&conv2).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));

        let conv3 = touched("/tmp");
        manager.save_conversation(&conv3).unwrap();

        let last = manager.load_last_conversation().unwrap().unwrap();
        assert_eq!(
            last.id, conv3.id,
            "should return the most-recently-written file"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_last_conversation_skips_corrupt_newest_falls_back_to_valid() {
        let dir = std::env::temp_dir().join("mermaid_test_conv_corrupt");
        let _ = fs::remove_dir_all(&dir);
        let manager = ConversationManager::new(&dir).unwrap();

        let good = touched("/tmp");
        manager.save_conversation(&good).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));

        // Plant a NEWER, corrupt file (well-formed name, garbage contents): the
        // newest-by-mtime entry is unparseable, so #68 must skip it.
        let corrupt = manager.conversations_dir().join("20991231_235959_999.json");
        fs::write(&corrupt, b"{ not valid json").unwrap();

        let last = manager.load_last_conversation().unwrap().unwrap();
        assert_eq!(
            last.id, good.id,
            "must fall back to the newest VALID conversation"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_last_conversation_none_when_only_corrupt() {
        let dir = std::env::temp_dir().join("mermaid_test_conv_only_corrupt");
        let _ = fs::remove_dir_all(&dir);
        let manager = ConversationManager::new(&dir).unwrap();
        fs::write(
            manager.conversations_dir().join("20991231_235959_998.json"),
            b"nope",
        )
        .unwrap();
        assert!(manager.load_last_conversation().unwrap().is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_conversation_tolerates_unknown_message_role() {
        // F74: a conversation written by a NEWER build may carry a MessageRole
        // this build doesn't model. It must still load — the unknown role maps to
        // a neutral System message — so `--continue` doesn't silently skip the
        // newest session (the prior behavior, when the whole parse hard-failed).
        let dir =
            std::env::temp_dir().join(format!("mermaid_conv_role_skew_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let manager = ConversationManager::new(&dir).unwrap();

        let id = "20260101_120000_001";
        let json = format!(
            r#"{{
                "id": "{id}",
                "title": "skew",
                "messages": [
                    {{
                        "role": "Developer",
                        "content": "from a newer build",
                        "timestamp": "2026-01-01T12:00:00-04:00"
                    }}
                ],
                "model_name": "m",
                "project_path": "/tmp",
                "created_at": "2026-01-01T12:00:00-04:00",
                "updated_at": "2026-01-01T12:00:00-04:00",
                "total_tokens": null
            }}"#
        );
        fs::write(manager.conversations_dir().join(format!("{id}.json")), json).unwrap();

        let loaded = manager
            .load_conversation(id)
            .expect("must load despite an unknown role");
        assert_eq!(loaded.messages().len(), 1);
        assert_eq!(
            loaded.messages()[0].role,
            MessageRole::System,
            "an unknown role becomes a neutral System message"
        );

        // And `--continue`'s newest-valid picker returns it instead of skipping.
        let last = manager
            .load_last_conversation()
            .unwrap()
            .expect("the newest session must load");
        assert_eq!(last.id, id);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_delete_conversation() {
        let dir = std::env::temp_dir().join("mermaid_test_conv_delete");
        let _ = fs::remove_dir_all(&dir);
        let manager = ConversationManager::new(&dir).unwrap();

        let conv = touched("/tmp");
        manager.save_conversation(&conv).unwrap();
        assert_eq!(manager.list_conversations().unwrap().len(), 1);

        manager.delete_conversation(&conv.id).unwrap();
        assert_eq!(manager.list_conversations().unwrap().len(), 0);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_session_is_not_saved() {
        let dir = std::env::temp_dir().join("mermaid_test_conv_empty_save");
        let _ = fs::remove_dir_all(&dir);
        let manager = ConversationManager::new(&dir).unwrap();

        // An untouched (message-less) conversation must not create a file.
        let mut conv = ConversationHistory::new("/tmp".into(), "m".into(), Local::now());
        manager.save_conversation(&conv).unwrap();
        assert!(
            manager.list_conversations().unwrap().is_empty(),
            "empty session must not be listed"
        );
        assert!(
            manager.load_last_conversation().unwrap().is_none(),
            "empty session must not be --continue-able"
        );

        // The first real message makes it persist.
        conv.add_messages(&[ChatMessage::user("hi")], Local::now());
        manager.save_conversation(&conv).unwrap();
        assert_eq!(manager.list_conversations().unwrap().len(), 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resume_paths_skip_pre_existing_empty_files() {
        // An empty session file planted directly on disk (e.g. saved before this
        // guard existed) must be invisible to the picker and to `--continue`.
        let dir = std::env::temp_dir().join("mermaid_test_conv_empty_resume");
        let _ = fs::remove_dir_all(&dir);
        let manager = ConversationManager::new(&dir).unwrap();

        let real = touched("/tmp");
        manager.save_conversation(&real).unwrap();
        // A NEWER empty file, written straight to disk to bypass the save guard.
        std::thread::sleep(std::time::Duration::from_millis(10));
        let empty = ConversationHistory::new("/tmp".into(), "m".into(), Local::now());
        let path = manager
            .conversations_dir()
            .join(format!("{}.json", empty.id));
        fs::write(&path, serde_json::to_string(&empty).unwrap()).unwrap();

        let list = manager.list_conversations().unwrap();
        assert_eq!(list.len(), 1, "the empty file must not be listed");
        assert_eq!(list[0].id, real.id);
        assert_eq!(
            manager.load_last_conversation().unwrap().unwrap().id,
            real.id,
            "--continue must skip the newer empty file"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_conversation_capped_refuses_oversized_file() {
        // #129: a file over the cap is refused before it's read into RAM. Use a
        // sparse file so the test stays fast and doesn't actually write 64 MiB.
        let dir = std::env::temp_dir().join(format!("mermaid_conv_cap_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let small = dir.join("small.json");
        fs::write(&small, b"{}").unwrap();
        assert!(read_conversation_capped(&small).is_ok());

        let big = dir.join("big.json");
        let f = fs::File::create(&big).unwrap();
        f.set_len(MAX_CONVERSATION_BYTES + 1).unwrap();
        assert!(
            read_conversation_capped(&big).is_err(),
            "a file over the cap must be refused, not slurped into memory"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    // The two tests that lived here pinned the snapshot-side F73 guard.
    // That guard moved to the append, so its coverage moved with it:
    // event_log::tests::a_second_writer_diverts_this_process_to_a_conflict_sibling.
    // Keeping them here would assert that a derived cache defends itself
    // against a writer that no longer races for it.
}
