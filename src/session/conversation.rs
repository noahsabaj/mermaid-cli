use crate::domain::CompactionArchive;
use crate::models::{ChatMessage, MessageRole};
use anyhow::Result;
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};

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
    pub total_tokens: Option<usize>,
    /// Metadata for context compactions performed in this conversation.
    #[serde(default)]
    pub compactions: Vec<crate::domain::CompactionRecord>,
    /// History of user input prompts for navigation (up/down arrows)
    #[serde(default)]
    pub input_history: VecDeque<String>,
}

impl ConversationHistory {
    /// Create a new conversation history
    pub fn new(project_path: String, model_name: String) -> Self {
        let now = Local::now();
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
            total_tokens: None,
            compactions: Vec::new(),
            input_history: VecDeque::new(),
        }
    }

    /// Add messages to the conversation
    pub fn add_messages(&mut self, messages: &[ChatMessage]) {
        self.messages.extend_from_slice(messages);
        self.updated_at = Local::now();
        self.update_title();
    }

    /// Replace the model-visible message log without deriving a new title.
    /// Used by context compaction: the original title still describes the
    /// session better than the generated checkpoint.
    pub fn replace_messages(&mut self, messages: Vec<ChatMessage>) {
        self.messages = messages;
        self.updated_at = Local::now();
    }

    /// Record a completed context compaction.
    pub fn add_compaction(&mut self, record: crate::domain::CompactionRecord) {
        self.compactions.push(record);
        self.updated_at = Local::now();
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

/// Manages conversation persistence for a project
#[derive(Clone)]
pub struct ConversationManager {
    conversations_dir: PathBuf,
    compactions_dir: PathBuf,
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
        })
    }

    /// Save a conversation to disk
    pub fn save_conversation(&self, conversation: &ConversationHistory) -> Result<()> {
        // The id field is persisted and round-trips through (potentially
        // tampered) on-disk state; validate it before it drives the write path,
        // so a loaded conversation can't escape the conversations dir on save.
        validate_conversation_id(&conversation.id)?;
        let filename = format!("{}.json", conversation.id);
        let path = self.conversations_dir.join(filename);

        // Strip computer-use screenshot bytes before they hit disk (#99). Only
        // clones the conversation when there is actually something to scrub.
        let json = match strip_persisted_screenshots(&conversation.messages) {
            Some(sanitized) => {
                let mut redacted = conversation.clone();
                redacted.messages = sanitized;
                serde_json::to_string_pretty(&redacted)?
            },
            None => serde_json::to_string_pretty(conversation)?,
        };
        // Atomic write: a crash mid-save must not empty/corrupt the session
        // file (this is the hot path, rewritten after nearly every message).
        crate::runtime::write_atomic(&path, json.as_bytes())?;

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
        // screenshot bytes here too so they don't survive in compaction archives
        // (#99). Clones only when a screenshot is actually present.
        let json = match strip_persisted_screenshots(&archive.messages) {
            Some(sanitized) => {
                let mut redacted = archive.clone();
                redacted.messages = sanitized;
                serde_json::to_string_pretty(&redacted)?
            },
            None => serde_json::to_string_pretty(archive)?,
        };
        // Atomic write: the archive is the ONLY durable copy of messages
        // dropped by a compaction — a partial write would lose them.
        crate::runtime::write_atomic(&path, json.as_bytes())?;
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
                {
                    conversations.push(conv);
                }
            }
        }

        // Sort by updated_at (newest first)
        conversations.sort_by_key(|c| std::cmp::Reverse(c.updated_at));

        Ok(conversations)
    }

    /// Delete a conversation
    pub fn delete_conversation(&self, id: &str) -> Result<()> {
        validate_conversation_id(id)?;
        let filename = format!("{}.json", id);
        let path = self.conversations_dir.join(filename);

        if path.exists() {
            fs::remove_file(path)?;
        }

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
        let mut conv = ConversationHistory::new("/tmp/p".into(), "m".into());
        conv.messages = vec![
            ChatMessage::user("u").with_images(vec!["USERIMG".to_string()]),
            ChatMessage::assistant("a").with_images(vec!["SHOTBYTES".to_string()]),
        ];
        let store = ConversationManager {
            conversations_dir: dir.clone(),
            compactions_dir: dir.clone(),
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
    fn test_new_conversation_has_session_title() {
        let conv = ConversationHistory::new("/tmp/project".into(), "test-model".into());
        assert!(conv.title.starts_with("Session "));
        assert_eq!(conv.model_name, "test-model");
        assert_eq!(conv.project_path, "/tmp/project");
        assert!(conv.messages.is_empty());
    }

    #[test]
    fn test_title_updates_from_first_user_message() {
        let mut conv = ConversationHistory::new("/tmp".into(), "m".into());
        conv.add_messages(&[ChatMessage::user("Fix the login bug")]);
        assert_eq!(conv.title, "Fix the login bug");
    }

    #[test]
    fn test_title_truncated_at_60_chars() {
        let mut conv = ConversationHistory::new("/tmp".into(), "m".into());
        let long_msg = "a".repeat(100);
        conv.add_messages(&[ChatMessage::user(long_msg)]);
        assert!(conv.title.ends_with("..."));
        assert!(conv.title.len() <= 64); // 60 chars + "..."
    }

    #[test]
    fn test_title_set_only_once() {
        let mut conv = ConversationHistory::new("/tmp".into(), "m".into());
        conv.add_messages(&[ChatMessage::user("First message")]);
        conv.add_messages(&[ChatMessage::user("Second message")]);
        assert_eq!(conv.title, "First message");
    }

    #[test]
    fn test_input_history_deduplication() {
        let mut conv = ConversationHistory::new("/tmp".into(), "m".into());
        conv.add_to_input_history("hello".into());
        conv.add_to_input_history("hello".into()); // duplicate
        conv.add_to_input_history("world".into());
        assert_eq!(conv.input_history.len(), 2);
    }

    #[test]
    fn test_input_history_skips_empty() {
        let mut conv = ConversationHistory::new("/tmp".into(), "m".into());
        conv.add_to_input_history("".into());
        conv.add_to_input_history("   ".into());
        assert_eq!(conv.input_history.len(), 0);
    }

    #[test]
    fn test_input_history_capped_at_100() {
        let mut conv = ConversationHistory::new("/tmp".into(), "m".into());
        for i in 0..110 {
            conv.add_to_input_history(format!("msg{}", i));
        }
        assert_eq!(conv.input_history.len(), 100);
        assert_eq!(conv.input_history.front().unwrap(), "msg10");
    }

    #[test]
    fn test_save_load_roundtrip() {
        let dir = std::env::temp_dir().join("mermaid_test_conv_roundtrip");
        let _ = fs::remove_dir_all(&dir);
        let manager = ConversationManager::new(&dir).unwrap();

        let mut conv = ConversationHistory::new("/tmp".into(), "model".into());
        conv.add_messages(&[ChatMessage::user("test message")]);
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

        let conv1 = ConversationHistory::new("/tmp".into(), "m".into());
        std::thread::sleep(std::time::Duration::from_millis(10));
        let conv2 = ConversationHistory::new("/tmp".into(), "m".into());

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

        let conv = ConversationHistory::new("/tmp".into(), "m".into());
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

        let conv1 = ConversationHistory::new("/tmp".into(), "m".into());
        manager.save_conversation(&conv1).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));

        let conv2 = ConversationHistory::new("/tmp".into(), "m".into());
        manager.save_conversation(&conv2).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));

        let conv3 = ConversationHistory::new("/tmp".into(), "m".into());
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

        let good = ConversationHistory::new("/tmp".into(), "m".into());
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
    fn test_delete_conversation() {
        let dir = std::env::temp_dir().join("mermaid_test_conv_delete");
        let _ = fs::remove_dir_all(&dir);
        let manager = ConversationManager::new(&dir).unwrap();

        let conv = ConversationHistory::new("/tmp".into(), "m".into());
        manager.save_conversation(&conv).unwrap();
        assert_eq!(manager.list_conversations().unwrap().len(), 1);

        manager.delete_conversation(&conv.id).unwrap();
        assert_eq!(manager.list_conversations().unwrap().len(), 0);

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
}
