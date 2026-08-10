//! The append-only session event log: one durable schema for session content.
//!
//! Design: `docs/design/event-log.md`. A session's committed history is a
//! sequence of [`SessionEvent`]s, one JSON line each, at
//! `.mermaid/conversations/<id>.jsonl`. The conversation snapshot stays the
//! resume authority; the log is the history behind it — [`fold_session`]
//! rebuilds the snapshot from the events, and the `fold == snapshot`
//! invariant test in `reducer.rs` is what keeps every transcript mutation
//! honest about emitting.
//!
//! Granularity is the *committed transcript*, not reducer inputs — the
//! opt-in `--record` trace already covers `Msg`-level replay and stays the
//! diagnostics instrument. One deliberate exclusion follows from that:
//! `RecoveryNudge` messages (per-dispatch steering notes, swept at turn end,
//! deliberately skipped by the snapshot save cadence too) never enter the
//! log, so a fold yields the durable transcript without live nudges.
//!
//! Purity: serde-only, no I/O, no wall clock — the appender (the effect
//! layer) owns the envelope's `seq`/`ts`; every content clock rides inside
//! an event (a `ChatMessage` carries its own timestamp, `compaction`/`reset`
//! carry theirs), so the fold is deterministic.

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

use crate::checklist::ChecklistStore;
use crate::compaction::CompactionEvent;
use crate::conversation::ConversationHistory;
use crate::state::{AdvertisedContext, ContextUsageSnapshot, PlanState, TokenUsageTotals};
use mermaid_model::action::ActionDisplay;
use mermaid_model::models::{ChatMessage, MessageRole};
use mermaid_model::safety::SafetyMode;

/// Bumped only on an incompatible change to the wire shape.
///
/// Additive variants and additive `ChatMessage` fields keep version 1
/// (readers deserialize unknown-to-them message fields via serde defaults).
/// Readers refuse newer versions, mirroring the recorder and the runtime DB.
pub const SESSION_EVENT_FORMAT_VERSION: u32 = 1;

/// One line of a session's `.jsonl` log: a transport envelope around one
/// typed event.
///
/// `seq` and `ts` are appender-owned metadata (truncation detection and a
/// late-attach cursor), never inputs to [`fold_session`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEventLine {
    /// [`SESSION_EVENT_FORMAT_VERSION`] at write time.
    pub v: u32,
    /// Per-file monotonic counter, starting at 0.
    pub seq: u64,
    /// Appender wall clock at write time.
    pub ts: DateTime<Local>,
    /// The content.
    pub event: SessionEvent,
}

/// One committed session fact. Internally tagged (`type`, `snake_case`),
/// matching the house style for stable wire unions (`RunEvent`,
/// `ToolMetadata`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEvent {
    /// Session identity — the first event of every log. Written by the
    /// appender (creation or backfill), never emitted by the reducer.
    Started {
        session_id: String,
        project_path: String,
        model_id: String,
        created_at: DateTime<Local>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        forked_from: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_session: Option<String>,
    },
    /// One message appended to the committed transcript. The message's own
    /// `timestamp` is the commit clock (`Session::append` stamps it), so the
    /// fold needs no envelope time.
    Message { message: ChatMessage },
    /// A message inserted just *before* the transcript's last entry — the
    /// mid-turn system-note path that must not split a trailing
    /// `tool_use` pair (`push_system`'s `would_split` branch).
    InsertedBeforeLast { message: ChatMessage },
    /// An action display attached to the last assistant message
    /// (`ToolFinished`). No index: "last assistant" is the live semantics,
    /// and it is stable under the nudge exclusion because a nudge is never
    /// an attach target. Boxed: `ActionDisplay` is the enum's largest
    /// payload by far, and serde treats the box as transparent.
    Action { action: Box<ActionDisplay> },
    /// A base64 image attached to the last assistant message (a tool's
    /// screenshot artifact routed onto the transcript).
    Image { data: String },
    /// A compaction replaced the model-visible transcript. `replacement` is
    /// the FINAL post-compaction transcript (including spliced-in messages
    /// that arrived mid-compaction); the archived originals are the earlier
    /// `message` events of this same log — append-only storage makes the
    /// archive a boundary, not a copy.
    Compaction {
        at: DateTime<Local>,
        record: CompactionEvent,
        replacement: Vec<ChatMessage>,
    },
    /// Wholesale transcript replacement outside compaction. Safety valve for
    /// structural operations the finer events cannot express.
    Reset {
        at: DateTime<Local>,
        messages: Vec<ChatMessage>,
    },
    /// The scalar session state, snapshot whole. Emitted at save points only
    /// when it differs from the previously emitted value, so the log carries
    /// one small line per actual change instead of a delta vocabulary.
    State(Box<SessionScalars>),
    /// One prompt-history entry (`input_history`). Emitted unconditionally
    /// at the record chokepoint; the fold routes it through the same
    /// deduplicating `add_to_input_history`, so both sides agree.
    Input { text: String },
    /// The task checklist, snapshot whole (it is already snapshot-shaped).
    /// Deduplicated at save points like `state`.
    Tasks { store: ChecklistStore },
}

/// Everything scalar the conversation snapshot carries — the fold assigns
/// these wholesale. `updated_at` is deliberately absent: the mutators stamp
/// it from event-carried clocks, exactly as live.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionScalars {
    pub title: String,
    pub model_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub safety_mode: Option<SafetyMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<PlanState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advertised_context: Option<AdvertisedContext>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_token_usage: Option<TokenUsageTotals>,
    #[serde(default)]
    pub cumulative_token_usage: TokenUsageTotals,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_usage: Option<ContextUsageSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cli_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
}

impl SessionScalars {
    /// Project a conversation snapshot's scalar state. The input is the
    /// SNAPSHOT (`Session::snapshot_conversation`), which already overlays
    /// the live meters and safety mode.
    #[must_use]
    pub fn of(snapshot: &ConversationHistory) -> Self {
        Self {
            title: snapshot.title.clone(),
            model_name: snapshot.model_name.clone(),
            safety_mode: snapshot.safety_mode,
            plan: snapshot.plan.clone(),
            advertised_context: snapshot.advertised_context.clone(),
            last_token_usage: snapshot.last_token_usage,
            cumulative_token_usage: snapshot.cumulative_token_usage,
            context_usage: snapshot.context_usage.clone(),
            git_branch: snapshot.git_branch.clone(),
            git_sha: snapshot.git_sha.clone(),
            cli_version: snapshot.cli_version.clone(),
            forked_from: snapshot.forked_from.clone(),
            parent_session: snapshot.parent_session.clone(),
        }
    }

    fn assign_to(&self, conversation: &mut ConversationHistory) {
        conversation.title.clone_from(&self.title);
        conversation.model_name.clone_from(&self.model_name);
        conversation.safety_mode = self.safety_mode;
        conversation.plan.clone_from(&self.plan);
        conversation
            .advertised_context
            .clone_from(&self.advertised_context);
        conversation.last_token_usage = self.last_token_usage;
        conversation.cumulative_token_usage = self.cumulative_token_usage;
        conversation.context_usage.clone_from(&self.context_usage);
        conversation.git_branch.clone_from(&self.git_branch);
        conversation.git_sha.clone_from(&self.git_sha);
        conversation.cli_version.clone_from(&self.cli_version);
        conversation.forked_from.clone_from(&self.forked_from);
        conversation.parent_session.clone_from(&self.parent_session);
    }
}

/// Rebuild a conversation snapshot from its event log.
///
/// Replays every event through the SAME mutators the live session uses
/// (`add_messages`, `replace_messages`, `add_compaction`,
/// `add_to_input_history`), so title derivation, `updated_at` stamping, and
/// input dedup cannot drift from live behavior. Returns `None` when the
/// stream does not begin with [`SessionEvent::Started`] — an empty or
/// truncated-at-birth log has no identity to build on.
///
/// The result equals the live snapshot up to the documented nudge exclusion:
/// a fold never contains `RecoveryNudge` messages, which the emission
/// chokepoints skip by design.
#[must_use]
pub fn fold_session(events: impl IntoIterator<Item = SessionEvent>) -> Option<ConversationHistory> {
    let mut events = events.into_iter();
    let mut conversation = match events.next()? {
        SessionEvent::Started {
            session_id,
            project_path,
            model_id,
            created_at,
            forked_from,
            parent_session,
        } => {
            let mut conversation = ConversationHistory::new(project_path, model_id, created_at);
            // The explicit id wins over the clock-derived one — a fork's
            // collision-bumped id must round-trip exactly.
            conversation.id = session_id;
            conversation.forked_from = forked_from;
            conversation.parent_session = parent_session;
            conversation
        },
        _ => return None,
    };
    for event in events {
        apply(&mut conversation, event);
    }
    Some(conversation)
}

/// Apply one event to a conversation under fold. A mid-stream `started` is
/// ignored (the appender never writes one, but a corrupt concatenation must
/// not reset identity).
fn apply(conversation: &mut ConversationHistory, event: SessionEvent) {
    match event {
        SessionEvent::Started { .. } => {},
        SessionEvent::Message { message } => {
            let at = message.timestamp;
            conversation.add_messages(&[message], at);
        },
        SessionEvent::InsertedBeforeLast { message } => {
            let messages = conversation.messages_mut();
            let pos = messages.len().saturating_sub(1);
            messages.insert(pos, message);
        },
        SessionEvent::Action { action } => {
            if let Some(last) = conversation.messages_mut().last_mut()
                && last.role == MessageRole::Assistant
            {
                last.actions.push(*action);
            }
        },
        SessionEvent::Image { data } => {
            if let Some(last) = conversation.messages_mut().last_mut()
                && last.role == MessageRole::Assistant
            {
                last.images.get_or_insert_with(Vec::new).push(data);
            }
        },
        SessionEvent::Compaction {
            at,
            record,
            replacement,
        } => {
            conversation.replace_messages(replacement, at);
            conversation.add_compaction(record, at);
        },
        SessionEvent::Reset { at, messages } => {
            conversation.replace_messages(messages, at);
        },
        SessionEvent::State(scalars) => scalars.assign_to(conversation),
        SessionEvent::Input { text } => conversation.add_to_input_history(text),
        SessionEvent::Tasks { store } => conversation.tasks = store,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_ts() -> DateTime<Local> {
        chrono::DateTime::parse_from_rfc3339("2026-07-02T12:00:00.123+00:00")
            .unwrap()
            .with_timezone(&Local)
    }

    fn started() -> SessionEvent {
        SessionEvent::Started {
            session_id: "20260702_120000_123".to_string(),
            project_path: "/tmp/proj".to_string(),
            model_id: "ollama/test".to_string(),
            created_at: fixed_ts(),
            forked_from: None,
            parent_session: None,
        }
    }

    fn stamped(mut msg: ChatMessage) -> ChatMessage {
        msg.timestamp = fixed_ts();
        msg
    }

    /// The compact variants' wire form is pinned exactly; message-bearing
    /// variants are pinned by tag + round-trip only, because they embed
    /// `ChatMessage`, whose schema may grow additively without a format
    /// bump (old readers default the new fields).
    #[test]
    fn compact_variant_wire_forms_are_frozen() {
        let input = SessionEvent::Input {
            text: "hello".to_string(),
        };
        assert_eq!(
            serde_json::to_string(&input).unwrap(),
            r#"{"type":"input","text":"hello"}"#
        );

        let tasks = SessionEvent::Tasks {
            store: ChecklistStore::default(),
        };
        assert_eq!(
            serde_json::to_string(&tasks).unwrap(),
            r#"{"type":"tasks","store":{"tasks":[],"next_id":0}}"#
        );

        let scalars = SessionScalars {
            title: "t".to_string(),
            model_name: "m".to_string(),
            safety_mode: None,
            plan: None,
            advertised_context: None,
            last_token_usage: None,
            cumulative_token_usage: TokenUsageTotals::default(),
            context_usage: None,
            git_branch: None,
            git_sha: None,
            cli_version: None,
            forked_from: None,
            parent_session: None,
        };
        assert_eq!(
            serde_json::to_string(&SessionEvent::State(Box::new(scalars))).unwrap(),
            r#"{"type":"state","title":"t","model_name":"m","cumulative_token_usage":{"prompt_tokens":0,"completion_tokens":0,"cached_input_tokens":0,"cache_creation_input_tokens":0,"reasoning_output_tokens":0}}"#
        );
    }

    #[test]
    fn envelope_and_started_round_trip() {
        let line = SessionEventLine {
            v: SESSION_EVENT_FORMAT_VERSION,
            seq: 7,
            ts: fixed_ts(),
            event: started(),
        };
        let wire = serde_json::to_string(&line).unwrap();
        assert!(wire.starts_with(r#"{"v":1,"seq":7,"#), "{wire}");
        assert!(wire.contains(r#""type":"started""#), "{wire}");
        let back: SessionEventLine = serde_json::from_str(&wire).unwrap();
        assert_eq!(format!("{back:?}"), format!("{line:?}"));
    }

    #[test]
    fn every_variant_round_trips() {
        let samples = vec![
            started(),
            SessionEvent::Message {
                message: stamped(ChatMessage::user("hi")),
            },
            SessionEvent::InsertedBeforeLast {
                message: stamped(ChatMessage::system("note")),
            },
            SessionEvent::Action {
                action: sample_action("read_file", "src/main.rs"),
            },
            SessionEvent::Image {
                data: "QUJD".to_string(),
            },
            SessionEvent::Compaction {
                at: fixed_ts(),
                record: sample_compaction_record(),
                replacement: vec![stamped(ChatMessage::system("checkpoint"))],
            },
            SessionEvent::Reset {
                at: fixed_ts(),
                messages: vec![stamped(ChatMessage::user("only"))],
            },
            SessionEvent::State(Box::new(SessionScalars::of(&ConversationHistory::new(
                "/p".to_string(),
                "m".to_string(),
                fixed_ts(),
            )))),
            SessionEvent::Input {
                text: "prompt".to_string(),
            },
            SessionEvent::Tasks {
                store: ChecklistStore::default(),
            },
        ];
        // One sample per variant — grows with the enum (the match below is
        // the compile-time reminder).
        for event in &samples {
            let tag = match event {
                SessionEvent::Started { .. } => "started",
                SessionEvent::Message { .. } => "message",
                SessionEvent::InsertedBeforeLast { .. } => "inserted_before_last",
                SessionEvent::Action { .. } => "action",
                SessionEvent::Image { .. } => "image",
                SessionEvent::Compaction { .. } => "compaction",
                SessionEvent::Reset { .. } => "reset",
                SessionEvent::State(_) => "state",
                SessionEvent::Input { .. } => "input",
                SessionEvent::Tasks { .. } => "tasks",
            };
            let wire = serde_json::to_string(event).unwrap();
            assert!(
                wire.contains(&format!(r#""type":"{tag}""#)),
                "tag drifted: {wire}"
            );
            let back: SessionEvent = serde_json::from_str(&wire).unwrap();
            assert_eq!(
                format!("{back:?}"),
                format!("{event:?}"),
                "round trip changed the event"
            );
        }
        assert_eq!(samples.len(), 10);
    }

    fn sample_action(action_type: &str, target: &str) -> Box<ActionDisplay> {
        Box::new(ActionDisplay {
            action_type: action_type.to_string(),
            target: target.to_string(),
            result: mermaid_model::action::ActionResult::Success {
                output: "ok".to_string(),
                images: None,
            },
            details: mermaid_model::action::ActionDetails::Simple,
            duration_seconds: Some(0.1),
            metadata: None,
        })
    }

    fn sample_compaction_record() -> CompactionEvent {
        CompactionEvent {
            id: "c1".to_string(),
            trigger: crate::compaction::CompactionTrigger::Manual,
            created_at: fixed_ts(),
            before_tokens: 1000,
            after_tokens: 100,
            archived_message_count: 8,
            preserved_message_count: 2,
            preserved_turn_count: 1,
            summary_tokens: 90,
            duration_secs: 1.5,
            review_status: crate::compaction::CompactionReviewStatus::Reviewed,
            review_error: None,
            focus: None,
            archive_path: None,
        }
    }

    #[test]
    fn fold_rebuilds_messages_title_and_scalars() {
        let user = stamped(ChatMessage::user("Fix the login bug"));
        let assistant = stamped(ChatMessage::assistant("on it"));
        let mut scalars = SessionScalars::of(&ConversationHistory::new(
            "/tmp/proj".to_string(),
            "ollama/test".to_string(),
            fixed_ts(),
        ));
        scalars.title = "Fix the login bug".to_string();
        scalars.git_branch = Some("main".to_string());
        scalars.cumulative_token_usage = TokenUsageTotals {
            prompt_tokens: 7,
            ..Default::default()
        };

        let folded = fold_session(vec![
            started(),
            SessionEvent::Message { message: user },
            SessionEvent::Message { message: assistant },
            SessionEvent::Input {
                text: "Fix the login bug".to_string(),
            },
            SessionEvent::State(Box::new(scalars)),
        ])
        .expect("started leads");

        assert_eq!(folded.id, "20260702_120000_123");
        assert_eq!(folded.messages().len(), 2);
        assert_eq!(folded.title, "Fix the login bug");
        assert_eq!(folded.git_branch.as_deref(), Some("main"));
        assert_eq!(folded.cumulative_token_usage.total_tokens(), 7);
        assert_eq!(folded.input_history.len(), 1);
        assert_eq!(folded.updated_at, fixed_ts());
    }

    #[test]
    fn fold_applies_attachments_to_the_last_assistant_only() {
        let folded = fold_session(vec![
            started(),
            SessionEvent::Message {
                message: stamped(ChatMessage::assistant("working")),
            },
            SessionEvent::Action {
                action: sample_action("execute_command", "ls"),
            },
            SessionEvent::Image {
                data: "QUJD".to_string(),
            },
            // A trailing user message: later attachments must no-op.
            SessionEvent::Message {
                message: stamped(ChatMessage::user("next")),
            },
            SessionEvent::Image {
                data: "WFla".to_string(),
            },
        ])
        .expect("folds");
        let messages = folded.messages();
        assert_eq!(messages[0].actions.len(), 1);
        assert_eq!(
            messages[0].images.as_deref(),
            Some(["QUJD".to_string()].as_slice())
        );
        assert!(messages[1].images.is_none(), "attach to a user is a no-op");
    }

    #[test]
    fn fold_compaction_replaces_and_records() {
        let checkpoint = stamped(ChatMessage::system("checkpoint"));
        let folded = fold_session(vec![
            started(),
            SessionEvent::Message {
                message: stamped(ChatMessage::user("old one")),
            },
            SessionEvent::Message {
                message: stamped(ChatMessage::user("old two")),
            },
            SessionEvent::Compaction {
                at: fixed_ts(),
                record: sample_compaction_record(),
                replacement: vec![checkpoint],
            },
        ])
        .expect("folds");
        assert_eq!(folded.messages().len(), 1);
        assert_eq!(folded.messages()[0].content, "checkpoint");
        assert_eq!(folded.compactions.len(), 1);
        assert_eq!(folded.compactions[0].id, "c1");
    }

    #[test]
    fn fold_inserted_before_last_lands_before_the_tail() {
        let folded = fold_session(vec![
            started(),
            SessionEvent::Message {
                message: stamped(ChatMessage::user("q")),
            },
            SessionEvent::Message {
                message: stamped(ChatMessage::assistant("calling tools")),
            },
            SessionEvent::InsertedBeforeLast {
                message: stamped(ChatMessage::system("server errored")),
            },
        ])
        .expect("folds");
        let contents: Vec<&str> = folded
            .messages()
            .iter()
            .map(|m| m.content.as_str())
            .collect();
        assert_eq!(contents, vec!["q", "server errored", "calling tools"]);
    }

    #[test]
    fn fold_without_leading_started_is_none() {
        assert!(
            fold_session(vec![SessionEvent::Input {
                text: "x".to_string()
            }])
            .is_none()
        );
        assert!(fold_session(Vec::new()).is_none());
    }

    #[test]
    fn fold_ignores_a_midstream_started() {
        let folded = fold_session(vec![
            started(),
            SessionEvent::Message {
                message: stamped(ChatMessage::user("kept")),
            },
            started(),
        ])
        .expect("folds");
        assert_eq!(folded.messages().len(), 1, "identity is not reset");
        assert_eq!(folded.id, "20260702_120000_123");
    }

    #[test]
    fn format_version_is_pinned() {
        assert_eq!(SESSION_EVENT_FORMAT_VERSION, 1);
    }

    /// THE completeness guard for `Session`'s emission chokepoints: drive
    /// every mutator, drain at two save points, and require the fold to
    /// reproduce the snapshot (JSON-compared — `revision` is `serde(skip)`
    /// session-only state and deliberately outside the contract). A mutator
    /// that forgets to emit turns this red.
    #[test]
    fn fold_matches_snapshot_across_session_mutations() {
        let mut state = crate::State::new(
            crate::Config::default(),
            std::path::PathBuf::from("/tmp/proj"),
            "ollama/test".to_string(),
            fixed_ts(),
            std::path::PathBuf::from("/tmp"),
        );
        let mut log: Vec<SessionEvent> = Vec::new();

        state
            .session
            .append(ChatMessage::user("hello there"), fixed_ts());
        state.session.record_input("hello there".to_string());
        state
            .session
            .append(ChatMessage::assistant("working on it"), fixed_ts());
        // Mid-turn note lands before the assistant tail (the would_split path).
        state
            .session
            .insert_before_last(stamped(ChatMessage::system("server notice")));
        state
            .session
            .attach_action(*sample_action("read_file", "src/lib.rs"));
        state.session.attach_image("QUJD".to_string());
        // A steering nudge: excluded from the log by design.
        let mut nudge = ChatMessage::system("plan reminder");
        nudge.kind = mermaid_model::models::ChatMessageKind::RecoveryNudge;
        state.session.append(nudge, fixed_ts());

        let snap1 = state.session.snapshot_conversation();
        log.extend(state.session.drain_events(&snap1));

        // Scalar change + more transcript, then a second drain.
        state.session.safety_mode = mermaid_model::safety::SafetyMode::FullAccess;
        state.session.cumulative_token_usage = TokenUsageTotals {
            prompt_tokens: 42,
            ..Default::default()
        };
        state
            .session
            .append(ChatMessage::user("and another"), fixed_ts());
        let snapshot = state.session.snapshot_conversation();
        log.extend(state.session.drain_events(&snapshot));

        let mut events = vec![SessionEvent::Started {
            session_id: snapshot.id.clone(),
            project_path: snapshot.project_path.clone(),
            model_id: snapshot.model_name.clone(),
            created_at: snapshot.created_at,
            forked_from: None,
            parent_session: None,
        }];
        events.extend(log);
        let folded = fold_session(events).expect("folds");

        // The documented exclusion: a fold carries no RecoveryNudge rows.
        let mut expected = snapshot.clone();
        let kept: Vec<ChatMessage> = expected
            .messages()
            .iter()
            .filter(|m| m.kind != mermaid_model::models::ChatMessageKind::RecoveryNudge)
            .cloned()
            .collect();
        assert_eq!(
            kept.len() + 1,
            snapshot.messages().len(),
            "the nudge must be present live for the exclusion to be exercised"
        );
        expected.set_messages(kept);

        assert_eq!(
            serde_json::to_value(&folded).unwrap(),
            serde_json::to_value(&expected).unwrap(),
            "fold(log) must equal the persisted snapshot (minus nudges)"
        );
    }

    #[test]
    fn drain_dedups_state_and_tasks_events() {
        let mut state = crate::State::new(
            crate::Config::default(),
            std::path::PathBuf::from("/tmp/proj"),
            "ollama/test".to_string(),
            fixed_ts(),
            std::path::PathBuf::from("/tmp"),
        );
        let snap = state.session.snapshot_conversation();
        let first = state.session.drain_events(&snap);
        assert!(
            matches!(
                first.as_slice(),
                [SessionEvent::State(_), SessionEvent::Tasks { .. }]
            ),
            "first drain emits both baselines: {first:?}"
        );
        // Nothing changed: the second drain is empty.
        let snap = state.session.snapshot_conversation();
        assert!(state.session.drain_events(&snap).is_empty());
        // A scalar change re-emits state (only).
        state.session.safety_mode = mermaid_model::safety::SafetyMode::ReadOnly;
        let snap = state.session.snapshot_conversation();
        let third = state.session.drain_events(&snap);
        assert!(
            matches!(third.as_slice(), [SessionEvent::State(_)]),
            "{third:?}"
        );
    }

    #[test]
    fn replace_conversation_clears_the_pending_buffer() {
        let mut state = crate::State::new(
            crate::Config::default(),
            std::path::PathBuf::from("/tmp/proj"),
            "ollama/test".to_string(),
            fixed_ts(),
            std::path::PathBuf::from("/tmp"),
        );
        state
            .session
            .append(ChatMessage::user("belongs to the old log"), fixed_ts());
        let next = ConversationHistory::new(
            "/tmp/proj".to_string(),
            "ollama/test".to_string(),
            fixed_ts(),
        );
        state.session.replace_conversation(next);
        let snap = state.session.snapshot_conversation();
        let drained = state.session.drain_events(&snap);
        assert!(
            !drained
                .iter()
                .any(|e| matches!(e, SessionEvent::Message { .. })),
            "the old conversation's message must not leak into the new log: {drained:?}"
        );
    }
}
