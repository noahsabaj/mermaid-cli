use crate::domain::CompactionArchive;
use crate::models::{ChatMessage, MessageRole};
use anyhow::Result;
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
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

/// A complete conversation history
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationHistory {
    pub id: String,
    pub title: String,
    pub messages: Vec<ChatMessage>,
    pub model_name: String,
    pub project_path: String,
    pub created_at: DateTime<Local>,
    pub updated_at: DateTime<Local>,
    /// Metadata for context compactions performed in this conversation.
    #[serde(default)]
    pub compactions: Vec<crate::domain::CompactionRecord>,
    /// History of user input prompts for navigation (up/down arrows)
    #[serde(default)]
    pub input_history: VecDeque<String>,
    /// Git branch this session was worked on, for the `--resume` picker.
    /// Captured at startup (impure) — `ConversationHistory::new` leaves it
    /// `None` so the pure reducer / `--replay` stay deterministic. Empty for
    /// non-git dirs and for sessions saved before this field existed.
    #[serde(default)]
    pub git_branch: Option<String>,
    /// Live session state that rides on `Session` (which is NOT serialized —
    /// only this `ConversationHistory` is), snapshotted here on every save via
    /// `Session::snapshot_conversation` so `--resume`/`--continue` restore the
    /// safety mode and token/context meters instead of resetting them. All
    /// `#[serde(default)]`: sessions saved before these existed omit them, and
    /// a `None` safety mode falls back to the config default on resume.
    #[serde(default)]
    pub safety_mode: Option<crate::runtime::SafetyMode>,
    #[serde(default)]
    pub last_token_usage: Option<crate::domain::TokenUsageTotals>,
    #[serde(default)]
    pub cumulative_token_usage: crate::domain::TokenUsageTotals,
    #[serde(default)]
    pub context_usage: Option<crate::domain::ContextUsageSnapshot>,
    /// Session lineage / provenance, all `#[serde(default)]` (older files omit
    /// them). Stamped in the impure startup path, never the pure reducer.
    /// `forked_from`/`parent_session` are set when a session is branched from
    /// another; `cli_version` and `git_sha` record the environment at creation.
    #[serde(default)]
    pub forked_from: Option<String>,
    #[serde(default)]
    pub parent_session: Option<String>,
    #[serde(default)]
    pub cli_version: Option<String>,
    #[serde(default)]
    pub git_sha: Option<String>,
    /// The task checklist (task_create/task_update tools + /tasks command).
    /// Snapshotted on every save so --resume/--continue restore an in-flight
    /// plan; sessions saved before this field existed load an empty store.
    #[serde(default)]
    pub tasks: crate::domain::TaskStore,
}

/// Best-effort current git branch of `dir`, for labelling `--resume` rows.
/// `None` when `dir` isn't a git work tree, git is absent, or HEAD is
/// detached. Kept out of the pure reducer — callers invoke it in the impure
/// startup path and stamp the result onto the conversation.
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
/// sessions doesn't have to parse every full transcript. The `<id>.json` stays
/// the source of truth; a session without a sidecar (older, or pre-sidecar) is
/// listed by fully parsing it.
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
            message_count: h.messages.len(),
            forked_from: h.forked_from.clone(),
        }
    }
}

impl ConversationHistory {
    /// Create a new conversation history.
    ///
    /// `now` is injected rather than read from the wall clock because this
    /// runs inside the pure reducer (`/clear` mints a fresh conversation, and
    /// `State::new` mints the initial one): the id and title are a
    /// deterministic function of the caller's clock, so `--replay` reproduces
    /// them exactly.
    pub fn new(project_path: String, model_name: String, now: DateTime<Local>) -> Self {
        // Include subsecond precision to avoid ID collisions within the same second
        let id = format!("{}", now.format("%Y%m%d_%H%M%S_%3f"));
        Self {
            id: id.clone(),
            title: format!("Session {}", now.format("%Y-%m-%d %H:%M")),
            messages: Vec::new(),
            model_name,
            project_path,
            created_at: now,
            updated_at: now,
            compactions: Vec::new(),
            input_history: VecDeque::new(),
            // Populated by the impure startup path (`detect_git_branch`), not
            // here — the reducer stays pure/deterministic for `--replay`.
            git_branch: None,
            // Snapshotted from `Session` on save (see `snapshot_conversation`).
            safety_mode: None,
            last_token_usage: None,
            cumulative_token_usage: crate::domain::TokenUsageTotals::default(),
            context_usage: None,
            // Lineage/provenance filled in by the impure startup path.
            forked_from: None,
            parent_session: None,
            cli_version: None,
            git_sha: None,
            tasks: crate::domain::TaskStore::default(),
        }
    }

    /// Add messages to the conversation. `now` is the caller's injected
    /// clock (`state.now` inside the reducer) — mutation methods never read
    /// the wall clock themselves so `update()` stays a pure function.
    pub fn add_messages(&mut self, messages: &[ChatMessage], now: DateTime<Local>) {
        self.messages.extend_from_slice(messages);
        self.updated_at = now;
        self.update_title();
    }

    /// Replace the model-visible message log without deriving a new title.
    /// Used by context compaction: the original title still describes the
    /// session better than the generated checkpoint. The messages keep their
    /// own timestamps (they rode in on the `Msg` payload); only `updated_at`
    /// is stamped, from the injected clock.
    pub fn replace_messages(&mut self, messages: Vec<ChatMessage>, now: DateTime<Local>) {
        self.messages = messages;
        self.updated_at = now;
    }

    /// Record a completed context compaction. `now` injected — see
    /// [`Self::add_messages`].
    pub fn add_compaction(
        &mut self,
        record: crate::domain::CompactionRecord,
        now: DateTime<Local>,
    ) {
        self.compactions.push(record);
        self.updated_at = now;
    }

    /// Add input to history (with deduplication of consecutive identical inputs)
    pub fn add_to_input_history(&mut self, input: String) {
        // Skip empty inputs
        if input.trim().is_empty() {
            return;
        }

        // Don't add if it's identical to the last entry
        if let Some(last) = self.input_history.back()
            && last == &input
        {
            return;
        }

        // Cap history at 100 entries to prevent unbounded growth
        if self.input_history.len() >= 100 {
            self.input_history.pop_front(); // O(1) instead of O(n)
        }

        self.input_history.push_back(input);
    }

    /// Update the title based on the first user message.
    /// Short-circuits if the title was already derived from a user message.
    fn update_title(&mut self) {
        // Only set title once — it comes from the first user message
        if !self.title.starts_with("Session ") {
            return;
        }
        if let Some(first_user_msg) = self.messages.iter().find(|m| m.role == MessageRole::User) {
            let preview = if first_user_msg.content.len() > 60 {
                let end = first_user_msg.content.floor_char_boundary(60);
                format!("{}...", &first_user_msg.content[..end])
            } else {
                first_user_msg.content.clone()
            };
            self.title = preview;
        }
    }

    /// Get a summary for display
    pub fn summary(&self) -> String {
        let message_count = self.messages.len();
        let duration = self.updated_at.signed_duration_since(self.created_at);
        let hours = duration.num_hours();
        let minutes = duration.num_minutes() % 60;

        format!(
            "{} | {} messages | {}h {}m | {}",
            self.updated_at.format("%Y-%m-%d %H:%M"),
            message_count,
            hours,
            minutes,
            self.title
        )
    }
}

/// Cheap fingerprint of a file on disk used for optimistic-concurrency
/// detection (F73): an `(mtime, len)` pair. A concurrent writer that rewrites a
/// conversation almost always changes the length (different message count) and
/// the mtime, so a mismatch against the value captured at load/last-save flags
/// the clobber without parsing the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    mtime: SystemTime,
    len: u64,
}

/// Stat `path` into a [`FileStamp`]. `None` when the file is absent/unreadable.
fn file_stamp(path: &Path) -> Option<FileStamp> {
    let meta = fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?;
    Some(FileStamp {
        mtime,
        len: meta.len(),
    })
}

/// Process-unique counter for `.conflict` sibling filenames so two conflicts on
/// the same id within one process don't collide.
static CONFLICT_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Manages conversation persistence for a project
#[derive(Clone)]
pub struct ConversationManager {
    conversations_dir: PathBuf,
    compactions_dir: PathBuf,
    /// Per-id `(mtime, len)` of the conversation file as THIS process last
    /// observed it — recorded at load and after each of our own saves. Used to
    /// detect a concurrent writer before `save_conversation` overwrites (F73).
    /// Shared across clones of the manager (same process) via the `Arc`, so a
    /// cloned manager sees the same baselines; separate processes have separate
    /// maps, which is exactly the cross-process clobber we want to catch.
    seen: Arc<Mutex<HashMap<String, FileStamp>>>,
}

impl ConversationManager {
    /// Create a new conversation manager for a project directory
    pub fn new(project_dir: impl AsRef<Path>) -> Result<Self> {
        let mermaid_dir = project_dir.as_ref().join(".mermaid");
        let conversations_dir = mermaid_dir.join("conversations");
        let compactions_dir = mermaid_dir.join("compactions");

        // Create conversations directory if it doesn't exist
        fs::create_dir_all(&conversations_dir)?;
        fs::create_dir_all(&compactions_dir)?;

        Ok(Self {
            conversations_dir,
            compactions_dir,
            seen: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Record the on-disk `(mtime, len)` of `path` as the baseline for `id`, so a
    /// later `save_conversation` can tell whether a concurrent writer touched the
    /// file since we read or wrote it (F73). Called on load and after our own
    /// saves. Best-effort: an unreadable file simply leaves no baseline.
    fn record_stamp(&self, id: &str, path: &Path) {
        if let Some(stamp) = file_stamp(path) {
            self.seen
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(id.to_string(), stamp);
        }
    }

    /// Path of a `.conflict` sibling for `id`. Deliberately ends in `.conflict`
    /// (not `.json`) so it never shows up in `list_conversations` /
    /// `load_last_conversation`, which only consider `*.json`. `id` is validated
    /// by the caller before this runs, so the filename can't traverse.
    fn conflict_sibling_path(&self, id: &str) -> PathBuf {
        let n = CONFLICT_COUNTER.fetch_add(1, Ordering::Relaxed);
        self.conversations_dir
            .join(format!("{}.{}.{}.conflict", id, std::process::id(), n))
    }

    /// Save a conversation to disk
    pub fn save_conversation(&self, conversation: &ConversationHistory) -> Result<()> {
        // The id field is persisted and round-trips through (potentially
        // tampered) on-disk state; validate it before it drives the write path,
        // so a loaded conversation can't escape the conversations dir on save.
        validate_conversation_id(&conversation.id)?;

        // An untouched session — the user ran `mermaid` and closed it without
        // sending anything — has no transcript. Never persist it, so it can't
        // clutter the `--resume` picker or be reached by `--continue`; the first
        // real message triggers the next save, which creates the file then.
        if conversation.messages.is_empty() {
            return Ok(());
        }

        let filename = format!("{}.json", conversation.id);
        let path = self.conversations_dir.join(filename);

        // Sanitize before persisting: strip computer-use screenshot bytes (#99)
        // AND scrub credential-shaped strings, so a persisted `read_file` of
        // `.env` or an API error echoing a key can't sit in cleartext (mirrors
        // the --record redaction in recorder.rs). Only clones when scrubbing.
        let mut value = match strip_persisted_screenshots(&conversation.messages) {
            Some(sanitized) => {
                let mut stripped = conversation.clone();
                stripped.messages = sanitized;
                serde_json::to_value(&stripped)?
            },
            None => serde_json::to_value(conversation)?,
        };
        crate::utils::redact_json(&mut value);
        let json = serde_json::to_string_pretty(&value)?;

        // Optimistic-concurrency guard (F73). Without this, two processes (e.g. a
        // daemon `run` and an interactive session) saving the same id do blind
        // last-writer-wins and silently clobber each other. If the file on disk
        // changed since we last read or wrote it — a concurrent writer — don't
        // overwrite: preserve our copy in a `.conflict` sibling and warn. The
        // baseline is per-process (`seen`), recorded at load and after our own
        // saves, so our OWN repeated saves don't false-positive.
        let baseline = self
            .seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&conversation.id)
            .copied();
        if let (Some(current), Some(base)) = (file_stamp(&path), baseline)
            && current != base
        {
            let sibling = self.conflict_sibling_path(&conversation.id);
            // Keep the existing atomic-write for the preserved copy too (owner-only).
            crate::runtime::write_atomic_with_mode(&sibling, json.as_bytes(), 0o600)?;
            tracing::warn!(
                id = %conversation.id,
                main = %path.display(),
                conflict = %sibling.display(),
                "conversation changed on disk since load (concurrent writer); wrote our copy to a .conflict sibling instead of overwriting"
            );
            return Ok(());
        }

        // Atomic write: a crash mid-save must not empty/corrupt the session
        // file (this is the hot path, rewritten after nearly every message).
        // Owner-only (0o600): the transcript can carry secrets in cleartext.
        crate::runtime::write_atomic_with_mode(&path, json.as_bytes(), 0o600)?;
        // Refresh our baseline to the file we just wrote so the NEXT save by this
        // process compares against our own write, not the pre-save state.
        self.record_stamp(&conversation.id, &path);

        // Write a tiny `<id>.meta` sidecar so `list_conversation_metas` can list
        // sessions without parsing this whole transcript. Best-effort: a failed
        // sidecar must never fail the save (the `.json` is the source of truth).
        let meta = ConversationMeta::from_history(conversation);
        if let Ok(meta_json) = serde_json::to_string(&meta) {
            let meta_path = self
                .conversations_dir
                .join(format!("{}.meta", conversation.id));
            let _ = crate::runtime::write_atomic_with_mode(&meta_path, meta_json.as_bytes(), 0o600);
        }

        Ok(())
    }

    /// Save the raw messages removed by a compaction. Archives live
    /// outside the hot conversation JSON so `/load` and `/list` don't
    /// parse old transcripts on every startup.
    pub fn save_compaction_archive(&self, archive: &CompactionArchive) -> Result<PathBuf> {
        // Both the conversation id (a directory component) and the archive id
        // (a file component) come from persisted state and must not traverse.
        validate_conversation_id(&archive.conversation_id)?;
        anyhow::ensure!(
            !archive.id.is_empty()
                && !archive.id.contains(['/', '\\'])
                && !archive.id.contains(".."),
            "invalid compaction archive id: {:?}",
            archive.id
        );
        let dir = self.compactions_dir.join(&archive.conversation_id);
        fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}.json", archive.id));
        // The archive is the only durable copy of compacted-out messages; scrub
        // screenshot bytes AND credential-shaped strings here too so neither
        // survives in compaction archives (#99). Clones only when scrubbing.
        let mut value = match strip_persisted_screenshots(&archive.messages) {
            Some(sanitized) => {
                let mut stripped = archive.clone();
                stripped.messages = sanitized;
                serde_json::to_value(&stripped)?
            },
            None => serde_json::to_value(archive)?,
        };
        crate::utils::redact_json(&mut value);
        let json = serde_json::to_string_pretty(&value)?;
        // Atomic write: the archive is the ONLY durable copy of messages
        // dropped by a compaction — a partial write would lose them. Owner-only.
        crate::runtime::write_atomic_with_mode(&path, json.as_bytes(), 0o600)?;
        Ok(path)
    }

    /// Load a specific conversation by ID
    pub fn load_conversation(&self, id: &str) -> Result<ConversationHistory> {
        validate_conversation_id(id)?;
        let filename = format!("{}.json", id);
        let path = self.conversations_dir.join(filename);

        let json = read_conversation_capped(&path)?;
        let conversation: ConversationHistory = serde_json::from_str(&json)?;
        // The file name was validated, but the deserialized `id` (which drives
        // later saves) is independent on-disk state — validate it too.
        validate_conversation_id(&conversation.id)?;

        // Capture the load-time baseline so a later save can detect a concurrent
        // writer that touched this file in between (F73).
        self.record_stamp(&conversation.id, &path);

        Ok(conversation)
    }

    /// Load the most recent *valid* conversation.
    ///
    /// Iterates files newest-first by mtime and returns the first that reads,
    /// parses, and has a valid id — skipping (with a warning) any unreadable,
    /// unparseable, or traversing-id file. Mirrors `list_conversations`'s
    /// tolerance so one corrupt/partial file (e.g. a crash mid-write) can't make
    /// `--continue` hard-fail; it falls back to the next-newest valid conversation.
    pub fn load_last_conversation(&self) -> Result<Option<ConversationHistory>> {
        let Ok(entries) = fs::read_dir(&self.conversations_dir) else {
            return Ok(None);
        };

        let mut candidates: Vec<(std::time::SystemTime, PathBuf)> = entries
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .filter_map(|e| {
                let mtime = e.metadata().ok()?.modified().ok()?;
                Some((mtime, e.path()))
            })
            .collect();
        candidates.sort_by_key(|(mtime, _)| std::cmp::Reverse(*mtime));

        for (_, path) in candidates {
            let Ok(json) = read_conversation_capped(&path) else {
                tracing::warn!(path = %path.display(), "skipping unreadable or oversized conversation file");
                continue;
            };
            let Ok(conv) = serde_json::from_str::<ConversationHistory>(&json) else {
                tracing::warn!(path = %path.display(), "skipping unparseable conversation file");
                continue;
            };
            // A planted session file with a traversing `id` must not become the
            // resumed conversation (its id would later drive an out-of-dir save).
            if validate_conversation_id(&conv.id).is_err() {
                tracing::warn!(path = %path.display(), id = %conv.id, "skipping conversation with invalid id");
                continue;
            }
            // Skip untouched (message-less) sessions — `--continue` resumes the
            // last chat with real history, not a blank one opened and closed.
            if conv.messages.is_empty() {
                continue;
            }
            // Capture the load-time baseline for the optimistic-concurrency
            // guard so a later save can detect a concurrent writer (F73).
            self.record_stamp(&conv.id, &path);
            return Ok(Some(conv));
        }
        Ok(None)
    }

    /// List all conversations in the project
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
                    && !conv.messages.is_empty()
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
    /// Newest-first. Cheaper than [`list_conversations`] for display-only paths.
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
                && !conv.messages.is_empty()
            {
                metas.push(ConversationMeta::from_history(&conv));
            }
        }
        metas.sort_by_key(|m| std::cmp::Reverse(m.updated_at));
        Ok(metas)
    }

    /// Delete a conversation (and its metadata sidecar).
    pub fn delete_conversation(&self, id: &str) -> Result<()> {
        validate_conversation_id(id)?;
        let path = self.conversations_dir.join(format!("{}.json", id));
        if path.exists() {
            fs::remove_file(path)?;
        }
        // Best-effort sidecar cleanup — its absence is harmless.
        let _ = fs::remove_file(self.conversations_dir.join(format!("{}.meta", id)));

        Ok(())
    }

    /// Get the conversations directory path
    pub fn conversations_dir(&self) -> &Path {
        &self.conversations_dir
    }

    pub fn compactions_dir(&self) -> &Path {
        &self.compactions_dir
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
            crate::domain::TokenUsageTotals::default()
        );
        assert!(conv.last_token_usage.is_none());
        assert!(conv.context_usage.is_none());
        assert!(conv.tasks.tasks.is_empty());
        assert_eq!(conv.tasks.next_id, 0);
    }

    #[test]
    fn tasks_round_trip_through_conversation_json() {
        let mut fresh = touched("/tmp/proj");
        fresh.tasks.create(
            vec![crate::domain::TaskSpec {
                subject: "wire broker".into(),
                active_form: "wiring broker".into(),
                description: Some("through ExecContext".into()),
                in_progress: true,
            }],
            crate::domain::TaskOrigin::Model,
            crate::domain::Stamp {
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
        conv.safety_mode = Some(crate::runtime::SafetyMode::FullAccess);
        conv.cumulative_token_usage = crate::domain::TokenUsageTotals {
            prompt_tokens: 777,
            ..Default::default()
        };
        let round: ConversationHistory =
            serde_json::from_str(&serde_json::to_string(&conv).unwrap()).unwrap();
        assert_eq!(
            round.safety_mode,
            Some(crate::runtime::SafetyMode::FullAccess)
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
        conv.messages = vec![
            ChatMessage::user("u").with_images(vec!["USERIMG".to_string()]),
            ChatMessage::assistant("a").with_images(vec!["SHOTBYTES".to_string()]),
        ];
        let store = ConversationManager {
            conversations_dir: dir.clone(),
            compactions_dir: dir.clone(),
            seen: Arc::new(Mutex::new(HashMap::new())),
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
            conv.messages[1].images.as_deref(),
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
        conv.messages = vec![
            ChatMessage::user("read .env"),
            ChatMessage::assistant("OPENAI_API_KEY=sk-abcdefghijklmnop1234"),
        ];
        let store = ConversationManager {
            conversations_dir: dir.clone(),
            compactions_dir: dir.clone(),
            seen: Arc::new(Mutex::new(HashMap::new())),
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
        assert!(conv.messages[1].content.contains("sk-abcdefghijklmnop1234"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_new_conversation_has_session_title() {
        let conv =
            ConversationHistory::new("/tmp/project".into(), "test-model".into(), Local::now());
        assert!(conv.title.starts_with("Session "));
        assert_eq!(conv.model_name, "test-model");
        assert_eq!(conv.project_path, "/tmp/project");
        assert!(conv.messages.is_empty());
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
            conv.add_to_input_history(format!("msg{}", i));
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
        assert_eq!(loaded.messages.len(), 1);
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
        assert_eq!(loaded.messages.len(), 1);
        assert_eq!(
            loaded.messages[0].role,
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

    #[test]
    fn save_conversation_detects_concurrent_writer_and_writes_conflict_sibling() {
        // F73: a daemon `run` and an interactive session can both hold the same
        // conversation id. Blind last-writer-wins silently drops one side's
        // edits. The optimistic-concurrency guard must detect the concurrent
        // write and preserve our copy in a `.conflict` sibling instead of
        // clobbering the other writer's file.
        let dir =
            std::env::temp_dir().join(format!("mermaid_conv_conflict_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let manager = ConversationManager::new(&dir).unwrap();

        let mut conv = ConversationHistory::new("/tmp".into(), "m".into(), Local::now());
        conv.add_messages(&[ChatMessage::user("ours")], Local::now());
        manager.save_conversation(&conv).unwrap();
        let main = manager
            .conversations_dir()
            .join(format!("{}.json", conv.id));

        // A SEPARATE process (its own baseline map) loads the same conversation,
        // appends, and saves — growing the file. This is the concurrent writer.
        let other = ConversationManager::new(&dir).unwrap();
        let mut their_conv = other.load_conversation(&conv.id).unwrap();
        their_conv.add_messages(
            &[ChatMessage::user("theirs - extra content here")],
            Local::now(),
        );
        other.save_conversation(&their_conv).unwrap();

        // Our next save still holds the pre-concurrent baseline, so it must NOT
        // overwrite the other writer's file.
        manager.save_conversation(&conv).unwrap();
        let on_disk: ConversationHistory =
            serde_json::from_str(&fs::read_to_string(&main).unwrap()).unwrap();
        assert_eq!(
            on_disk.messages.len(),
            2,
            "the concurrent writer's file must be left intact"
        );

        // Our copy is preserved in exactly one `.conflict` sibling.
        let mut conflicts = fs::read_dir(manager.conversations_dir())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".conflict"))
            .map(|e| e.path())
            .collect::<Vec<_>>();
        assert_eq!(conflicts.len(), 1, "exactly one .conflict sibling expected");
        let sibling = fs::read_to_string(conflicts.pop().unwrap()).unwrap();
        assert!(
            sibling.contains("ours") && !sibling.contains("theirs"),
            "the .conflict sibling holds OUR copy, not the concurrent writer's"
        );

        // The `.conflict` sibling must not pollute the conversation listing
        // (it isn't a `*.json` file).
        let listed = manager.list_conversations().unwrap();
        assert_eq!(
            listed.len(),
            1,
            ".conflict sibling must not appear as a conversation"
        );
        assert_eq!(listed[0].id, conv.id);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_conversation_repeated_self_saves_do_not_conflict() {
        // The guard must not false-positive on a single process's OWN repeated
        // saves (the hot path rewrites the file after nearly every message).
        let dir =
            std::env::temp_dir().join(format!("mermaid_conv_self_save_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let manager = ConversationManager::new(&dir).unwrap();

        let mut conv = ConversationHistory::new("/tmp".into(), "m".into(), Local::now());
        conv.add_messages(&[ChatMessage::user("first")], Local::now());
        manager.save_conversation(&conv).unwrap();
        conv.add_messages(&[ChatMessage::user("second")], Local::now());
        manager.save_conversation(&conv).unwrap();

        let conflicts = fs::read_dir(manager.conversations_dir())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".conflict"))
            .count();
        assert_eq!(
            conflicts, 0,
            "our own repeated saves must not be flagged as conflicts"
        );
        let loaded = manager.load_conversation(&conv.id).unwrap();
        assert_eq!(loaded.messages.len(), 2, "latest save must win for us");

        let _ = fs::remove_dir_all(&dir);
    }
}
