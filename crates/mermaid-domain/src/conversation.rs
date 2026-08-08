//! The conversation value: messages, usage totals, checklist, plan state.
//!
//! Pure data, and every field is already a domain type (`CompactionEvent`,
//! `PlanState`, `ChecklistStore`, `TokenUsageTotals`), so this is a downward
//! move rather than a new dependency.
//!
//! It lived in `src/session/`, in the same file as `ConversationManager`'s
//! `std::fs` calls and its two `Command::new("git")` spawns — while being a
//! field of `State`, a `Cmd` payload and a `Msg` payload. So the pure core
//! depended on a module that does real I/O, and the purity guard passed anyway
//! because it greps the text of `src/domain/**` and the I/O was one type-hop
//! away. Reading and writing a conversation stays in `src/session/`.

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

use mermaid_model::models::{ChatMessage, MessageRole};

/// A complete conversation history
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationHistory {
    pub id: String,
    pub title: String,
    /// PRIVATE on purpose. Every mutation must go through
    /// [`ConversationHistory::messages_mut`], which bumps [`Self::revision`] —
    /// the render layer's memo keys off that counter instead of re-hashing the
    /// whole transcript on every frame. A public field would make the counter a
    /// convention someone eventually forgets; this makes forgetting impossible
    /// from outside this module.
    messages: Vec<ChatMessage>,
    /// Bumped on every mutable access to `messages`. Session-only (never
    /// serialized): a resumed conversation starts at 0 with an empty render
    /// memo, so no stale key can survive a reload.
    #[serde(skip)]
    revision: u64,
    pub model_name: String,
    pub project_path: String,
    pub created_at: DateTime<Local>,
    pub updated_at: DateTime<Local>,
    /// Metadata for context compactions performed in this conversation.
    #[serde(default)]
    pub compactions: Vec<crate::CompactionEvent>,
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
    pub safety_mode: Option<mermaid_runtime::SafetyMode>,
    /// Plan-mode-in-progress (see `domain::PlanState`): `Some` only when the
    /// session was saved mid-planning, so `--resume` re-enters plan mode with
    /// the same plan file and restore target.
    #[serde(default)]
    pub plan: Option<crate::PlanState>,
    /// The mode-defining facts the model was last told about (see
    /// `domain::AdvertisedContext`) — the dispatch-time context-delta
    /// injector's baseline. Rides the conversation (not `Session`) so
    /// save/resume, `/clear`, and forks inherit the right baseline for
    /// free. `None` on fresh conversations and pre-field saves: the first
    /// dispatch stamps it silently instead of announcing current state.
    #[serde(default)]
    pub advertised_context: Option<crate::AdvertisedContext>,
    #[serde(default)]
    pub last_token_usage: Option<crate::TokenUsageTotals>,
    #[serde(default)]
    pub cumulative_token_usage: crate::TokenUsageTotals,
    #[serde(default)]
    pub context_usage: Option<crate::ContextUsageSnapshot>,
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
    /// The task checklist (`task_create/task_update` tools + /tasks command).
    /// Snapshotted on every save so --resume/--continue restore an in-flight
    /// plan; sessions saved before this field existed load an empty store.
    #[serde(default)]
    pub tasks: crate::ChecklistStore,
}

impl ConversationHistory {
    /// Read the committed transcript.
    #[must_use]
    pub fn messages(&self) -> &[ChatMessage] {
        &self.messages
    }

    /// Mutable access to the transcript. Bumps [`Self::revision`], which is
    /// what lets the render memo skip re-hashing every message on every frame.
    ///
    /// Deliberately conservative: it bumps on ACCESS, not on actual change, so
    /// a caller that takes the handle and changes nothing merely costs one memo
    /// miss. The reverse error — a change that does not bump — would paint a
    /// stale transcript, so the cheap direction is the safe one.
    pub fn messages_mut(&mut self) -> &mut Vec<ChatMessage> {
        self.revision = self.revision.wrapping_add(1);
        &mut self.messages
    }

    /// Replace the transcript wholesale (compaction, `/clear`, fork, load).
    pub fn set_messages(&mut self, messages: Vec<ChatMessage>) {
        self.revision = self.revision.wrapping_add(1);
        self.messages = messages;
    }

    /// Monotonic counter identifying the current transcript contents. Changes
    /// whenever `messages` is touched; never serialized.
    #[must_use]
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Create a new conversation history.
    ///
    /// `now` is injected rather than read from the wall clock because this
    /// runs inside the pure reducer (`/clear` mints a fresh conversation, and
    /// `State::new` mints the initial one): the id and title are a
    /// deterministic function of the caller's clock, so `--replay` reproduces
    /// them exactly.
    #[must_use]
    pub fn new(project_path: String, model_name: String, now: DateTime<Local>) -> Self {
        // Include subsecond precision to avoid ID collisions within the same second
        let id = format!("{}", now.format("%Y%m%d_%H%M%S_%3f"));
        Self {
            id: id.clone(),
            title: format!("Session {}", now.format("%Y-%m-%d %H:%M")),
            messages: Vec::new(),
            revision: 0,
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
            plan: None,
            // Stamped by the injector at the first dispatch (silent seed).
            advertised_context: None,
            last_token_usage: None,
            cumulative_token_usage: crate::TokenUsageTotals::default(),
            context_usage: None,
            // Lineage/provenance filled in by the impure startup path.
            forked_from: None,
            parent_session: None,
            cli_version: None,
            git_sha: None,
            tasks: crate::ChecklistStore::default(),
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
    pub fn add_compaction(&mut self, record: crate::CompactionEvent, now: DateTime<Local>) {
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
    #[must_use]
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
