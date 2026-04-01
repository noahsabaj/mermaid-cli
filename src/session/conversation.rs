use crate::models::{ChatMessage, MessageRole};
use anyhow::Result;
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};

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
            input_history: VecDeque::new(),
        }
    }

    /// Add messages to the conversation
    pub fn add_messages(&mut self, messages: &[ChatMessage]) {
        self.messages.extend_from_slice(messages);
        self.updated_at = Local::now();
        self.update_title();
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
}

impl ConversationManager {
    /// Create a new conversation manager for a project directory
    pub fn new(project_dir: impl AsRef<Path>) -> Result<Self> {
        let conversations_dir = project_dir.as_ref().join(".mermaid").join("conversations");

        // Create conversations directory if it doesn't exist
        fs::create_dir_all(&conversations_dir)?;

        Ok(Self { conversations_dir })
    }

    /// Save a conversation to disk
    pub fn save_conversation(&self, conversation: &ConversationHistory) -> Result<()> {
        let filename = format!("{}.json", conversation.id);
        let path = self.conversations_dir.join(filename);

        let json = serde_json::to_string_pretty(conversation)?;
        fs::write(path, json)?;

        Ok(())
    }

    /// Load a specific conversation by ID
    pub fn load_conversation(&self, id: &str) -> Result<ConversationHistory> {
        let filename = format!("{}.json", id);
        let path = self.conversations_dir.join(filename);

        let json = fs::read_to_string(path)?;
        let conversation: ConversationHistory = serde_json::from_str(&json)?;

        Ok(conversation)
    }

    /// Load the most recent conversation
    pub fn load_last_conversation(&self) -> Result<Option<ConversationHistory>> {
        let conversations = self.list_conversations()?;

        if conversations.is_empty() {
            return Ok(None);
        }

        // Conversations are already sorted by modification time (newest first)
        Ok(conversations.into_iter().next())
    }

    /// List all conversations in the project
    pub fn list_conversations(&self) -> Result<Vec<ConversationHistory>> {
        let mut conversations = Vec::new();

        // Read all JSON files in the conversations directory
        if let Ok(entries) = fs::read_dir(&self.conversations_dir) {
            for entry in entries.flatten() {
                if let Some(ext) = entry.path().extension()
                    && ext == "json"
                    && let Ok(json) = fs::read_to_string(entry.path())
                    && let Ok(conv) = serde_json::from_str::<ConversationHistory>(&json)
                {
                    conversations.push(conv);
                }
            }
        }

        // Sort by updated_at (newest first)
        conversations.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));

        Ok(conversations)
    }

    /// Delete a conversation
    pub fn delete_conversation(&self, id: &str) -> Result<()> {
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
