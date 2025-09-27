use crate::models::{ChatMessage, MessageRole};
use anyhow::Result;
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
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
}

impl ConversationHistory {
    /// Create a new conversation history
    pub fn new(project_path: String, model_name: String) -> Self {
        let now = Local::now();
        let id = format!("{}", now.format("%Y%m%d_%H%M%S"));
        Self {
            id: id.clone(),
            title: format!("Session {}", now.format("%Y-%m-%d %H:%M")),
            messages: Vec::new(),
            model_name,
            project_path,
            created_at: now,
            updated_at: now,
            total_tokens: None,
        }
    }

    /// Add messages to the conversation
    pub fn add_messages(&mut self, messages: &[ChatMessage]) {
        self.messages.extend_from_slice(messages);
        self.updated_at = Local::now();
        self.update_title();
    }

    /// Update the title based on the first user message
    fn update_title(&mut self) {
        if let Some(first_user_msg) = self.messages.iter().find(|m| m.role == MessageRole::User) {
            // Take first 60 chars of first user message as title
            let preview = if first_user_msg.content.len() > 60 {
                format!("{}...", &first_user_msg.content[..60])
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
pub struct ConversationManager {
    #[allow(dead_code)]
    project_dir: PathBuf,
    conversations_dir: PathBuf,
}

impl ConversationManager {
    /// Create a new conversation manager for a project directory
    pub fn new(project_dir: impl AsRef<Path>) -> Result<Self> {
        let project_dir = project_dir.as_ref().to_path_buf();
        let conversations_dir = project_dir.join(".mermaid").join("conversations");

        // Create conversations directory if it doesn't exist
        fs::create_dir_all(&conversations_dir)?;

        Ok(Self {
            project_dir,
            conversations_dir,
        })
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
        Ok(Some(conversations.into_iter().next().unwrap()))
    }

    /// List all conversations in the project
    pub fn list_conversations(&self) -> Result<Vec<ConversationHistory>> {
        let mut conversations = Vec::new();

        // Read all JSON files in the conversations directory
        if let Ok(entries) = fs::read_dir(&self.conversations_dir) {
            for entry in entries.flatten() {
                if let Some(ext) = entry.path().extension() {
                    if ext == "json" {
                        if let Ok(json) = fs::read_to_string(entry.path()) {
                            if let Ok(conv) = serde_json::from_str::<ConversationHistory>(&json) {
                                conversations.push(conv);
                            }
                        }
                    }
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
