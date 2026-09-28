//! Tools over the model's own context: `context_archive` searches and pages
//! the session log, `compact_context` asks for a checkpoint.
//!
//! Compaction removes messages from the working context, but nothing is
//! deleted: every message the session committed is a `message` event in its
//! `.jsonl` log, and a compaction only appends a boundary. So the checkpoint
//! summary is not a single point of failure — when it left something out, the
//! model searches the log and reads the original back, whole.
//!
//! `compact_context` lets the model decide when to checkpoint rather than
//! waiting for the fill threshold, which stays as the safety net. The tool
//! itself only records the ask (`ToolMetadata::CompactionRequest`); the
//! reducer carries it onto the next request and the model-call path compacts
//! before sending.

use std::fmt::Write as _;
use std::time::Instant;

use async_trait::async_trait;

use mermaid_domain::{SessionEvent, ToolDefinition, ToolMetadata, ToolOutcome, ToolRunMetadata};
use mermaid_model::models::{ChatMessage, MessageRole};

use super::super::ctx::ExecContext;
use super::ToolExecutor;

/// Ceiling on one `context_archive` result. Past it, results page.
const ARCHIVE_OUTPUT_MAX_CHARS: usize = 32_000;
/// Default and ceiling for `limit` on search and list.
const ARCHIVE_DEFAULT_LIMIT: usize = 20;
const ARCHIVE_MAX_LIMIT: usize = 200;
/// Characters of context either side of a search hit.
const SNIPPET_RADIUS: usize = 160;
/// Characters of each message shown when listing.
const LIST_PREVIEW_CHARS: usize = 200;

pub struct ContextArchiveTool;

pub struct CompactContextTool;

/// One message from the log, numbered in the order it was committed.
struct ArchivedMessage {
    number: usize,
    /// Id of the compaction that moved it out of the working context, if one
    /// has since run.
    compacted_by: Option<String>,
    message: ChatMessage,
}

impl ArchivedMessage {
    fn heading(&self) -> String {
        let role = match self.message.role {
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::System => "system",
            MessageRole::Tool => "tool",
        };
        let mut heading = format!("#{} {role}", self.number);
        if let Some(name) = &self.message.tool_name {
            let _ = write!(heading, " ({name})");
        }
        if let Some(id) = &self.compacted_by {
            let _ = write!(heading, " [compacted by {id}]");
        }
        heading
    }

    /// Everything searchable about the message: its text plus the calls it
    /// made, since a file write lives in the arguments, not the content.
    fn text(&self) -> String {
        let mut text = self.message.content.clone();
        for call in self.message.tool_calls.iter().flatten() {
            let _ = write!(
                text,
                "\n[tool_call {} {}]",
                call.function.name, call.function.arguments
            );
        }
        text
    }
}

/// Number the committed messages of a session log in commit order and mark
/// which a later compaction moved out of the working context.
///
/// A log opens with a `reset` carrying the transcript as it stood when the
/// log was created (and a rewind writes another), so a reset contributes the
/// messages it holds that the archive has not seen yet. Messages a reset or a
/// compaction dropped stay: they happened, and that is what the archive is
/// for.
fn archived_messages(events: Vec<SessionEvent>) -> Vec<ArchivedMessage> {
    use mermaid_domain::CompactionBoundary;
    let mut out: Vec<ArchivedMessage> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let push = |out: &mut Vec<ArchivedMessage>,
                seen: &mut std::collections::HashSet<String>,
                message: ChatMessage| {
        seen.insert(CompactionBoundary::fingerprint_of(&message));
        out.push(ArchivedMessage {
            number: out.len() + 1,
            compacted_by: None,
            message,
        });
    };
    for event in events {
        match event {
            SessionEvent::Message { message } | SessionEvent::InsertedBeforeLast { message } => {
                push(&mut out, &mut seen, message);
            },
            SessionEvent::Reset { messages, .. } => {
                for message in messages {
                    if !seen.contains(&CompactionBoundary::fingerprint_of(&message)) {
                        push(&mut out, &mut seen, message);
                    }
                }
            },
            SessionEvent::Compaction {
                record,
                replacement,
                ..
            } => {
                // Whatever the replacement kept verbatim is still live; every
                // earlier message not already claimed by a compaction went.
                let kept: std::collections::HashSet<String> = replacement
                    .iter()
                    .map(CompactionBoundary::fingerprint_of)
                    .collect();
                for entry in &mut out {
                    if entry.compacted_by.is_none()
                        && !kept.contains(&CompactionBoundary::fingerprint_of(&entry.message))
                    {
                        entry.compacted_by = Some(record.id.clone());
                    }
                }
            },
            SessionEvent::Started { .. }
            | SessionEvent::Action { .. }
            | SessionEvent::Image { .. }
            | SessionEvent::State(_)
            | SessionEvent::Input { .. }
            | SessionEvent::Tasks { .. } => {},
        }
    }
    out
}

fn usize_arg(args: &serde_json::Value, key: &str) -> Option<usize> {
    args.get(key)
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
}

fn str_arg<'a>(args: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// `text[start..end]` by characters, with ellipses where it was cut.
fn char_window(text: &str, start: usize, end: usize) -> String {
    let total = text.chars().count();
    let end = end.min(total);
    let start = start.min(end);
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.extend(text.chars().skip(start).take(end - start));
    if end < total {
        out.push('…');
    }
    out
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Case-insensitive search. Returns the rendered hits and how many matched.
fn search(messages: &[ArchivedMessage], query: &str, from: usize, limit: usize) -> (String, usize) {
    let needle = query.to_lowercase();
    let mut out = String::new();
    let mut matched = 0usize;
    let mut shown = 0usize;
    let mut next_from = None;
    for entry in messages.iter().filter(|m| m.number >= from) {
        let text = entry.text();
        let lower = text.to_lowercase();
        let Some(byte_at) = lower.find(&needle) else {
            continue;
        };
        matched += 1;
        if shown >= limit || out.len() >= ARCHIVE_OUTPUT_MAX_CHARS {
            next_from.get_or_insert(entry.number);
            continue;
        }
        // Lowercasing can change byte lengths, so locate the hit by chars.
        let char_at = lower[..byte_at].chars().count();
        let hits = lower.matches(&needle).count();
        let snippet = one_line(&char_window(
            &text,
            char_at.saturating_sub(SNIPPET_RADIUS),
            char_at + needle.chars().count() + SNIPPET_RADIUS,
        ));
        let _ = writeln!(
            out,
            "{} — {hits} hit{}\n  {snippet}",
            entry.heading(),
            if hits == 1 { "" } else { "s" }
        );
        shown += 1;
    }
    if matched == 0 {
        return (format!("No messages match \"{query}\"."), 0);
    }
    let mut header = format!("{matched} message(s) match \"{query}\"");
    if let Some(next) = next_from {
        let _ = write!(header, "; showing {shown}, pass from={next} for more");
    }
    header.push_str(". Read one whole with `message`.\n\n");
    (header + &out, matched)
}

fn list(messages: &[ArchivedMessage], from: usize, limit: usize) -> (String, usize) {
    let mut out = format!(
        "{} message(s) in this session's log. Read one whole with `message`.\n\n",
        messages.len()
    );
    let mut shown = 0usize;
    for entry in messages.iter().filter(|m| m.number >= from) {
        if shown >= limit || out.len() >= ARCHIVE_OUTPUT_MAX_CHARS {
            let _ = write!(
                out,
                "\nMore from #{}: pass from={}.",
                entry.number, entry.number
            );
            break;
        }
        let preview = one_line(&char_window(&entry.text(), 0, LIST_PREVIEW_CHARS));
        let _ = writeln!(out, "{}: {preview}", entry.heading());
        shown += 1;
    }
    (out, shown)
}

fn read(messages: &[ArchivedMessage], number: usize, char_offset: usize) -> Result<String, String> {
    let entry = messages
        .iter()
        .find(|m| m.number == number)
        .ok_or_else(|| {
            format!(
                "No message #{number}; this session's log holds #1 to #{}.",
                messages.len()
            )
        })?;
    let text = entry.text();
    let total = text.chars().count();
    let end = char_offset
        .saturating_add(ARCHIVE_OUTPUT_MAX_CHARS)
        .min(total);
    let body: String = text
        .chars()
        .skip(char_offset)
        .take(end.saturating_sub(char_offset))
        .collect();
    let mut out = format!("{} — {total} chars", entry.heading());
    if char_offset > 0 || end < total {
        let _ = write!(out, ", showing {char_offset}..{end}");
        if end < total {
            let _ = write!(out, "; pass char_offset={end} for the rest");
        }
    }
    out.push_str("\n\n");
    out.push_str(&body);
    Ok(out)
}

#[async_trait]
impl ToolExecutor for ContextArchiveTool {
    fn name(&self) -> &'static str {
        "context_archive"
    }

    fn schema(&self) -> ToolDefinition {
        ToolDefinition {
            name: "context_archive".to_string(),
            description: "Search or read this session's full message history, including \
                everything compaction removed from your context. Messages are numbered in the \
                order they happened and kept whole, tool output included. Pass `query` to find \
                messages containing a string (case-insensitive), `message` to read one in full, \
                or neither to list them."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Text to search for, case-insensitive."
                    },
                    "message": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Number of one message to read in full."
                    },
                    "char_offset": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "With `message`: where to resume a long message."
                    },
                    "from": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Search or list from this message number on."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": ARCHIVE_MAX_LIMIT,
                        "description": "Most messages to return when searching or listing."
                    }
                }
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, ctx: ExecContext) -> ToolOutcome {
        let started = Instant::now();
        let secs = || started.elapsed().as_secs_f64();
        let Some(session_id) = ctx.session_id.clone() else {
            return ToolOutcome::error(
                "This run keeps no session log, so there is no archive to search.",
                Some(secs()),
            );
        };
        let workdir = ctx.workdir.clone();
        let loaded = tokio::task::spawn_blocking(move || {
            crate::session::ConversationManager::new(&workdir)
                .and_then(|manager| manager.read_session_events(&session_id))
        })
        .await;
        let events = match loaded {
            Ok(Ok(Some(events))) => events,
            Ok(Ok(None)) => Vec::new(),
            Ok(Err(error)) => {
                return ToolOutcome::error(
                    format!("Could not read the session log: {error}"),
                    Some(secs()),
                );
            },
            Err(error) => {
                return ToolOutcome::error(
                    format!("Reading the session log failed: {error}"),
                    Some(secs()),
                );
            },
        };
        let messages = archived_messages(events);
        if messages.is_empty() {
            return ToolOutcome::success("The session log holds no messages yet.", "empty", secs());
        }

        let query = str_arg(&args, "query");
        let from = usize_arg(&args, "from").unwrap_or(1).max(1);
        let limit = usize_arg(&args, "limit")
            .unwrap_or(ARCHIVE_DEFAULT_LIMIT)
            .clamp(1, ARCHIVE_MAX_LIMIT);
        let (output, summary, result_count) = if let Some(number) = usize_arg(&args, "message") {
            match read(
                &messages,
                number,
                usize_arg(&args, "char_offset").unwrap_or(0),
            ) {
                Ok(output) => (output, format!("message {number}"), 1),
                Err(error) => return ToolOutcome::error(error, Some(secs())),
            }
        } else if let Some(query) = query {
            let (output, matched) = search(&messages, query, from, limit);
            (output, format!("{matched} matched"), matched)
        } else {
            let (output, shown) = list(&messages, from, limit);
            (output, format!("{shown} listed"), shown)
        };
        // The log holds what the model saw, but this result is committed and
        // persisted again; scrub anything credential-shaped on the way out.
        let output = mermaid_model::utils::redact_secrets(&output);
        ToolOutcome::success(output, summary, secs()).with_metadata(ToolRunMetadata {
            detail: ToolMetadata::ContextArchive {
                query: query.map(mermaid_model::utils::redact_secrets),
                result_count,
            },
            result_count: Some(result_count),
            ..ToolRunMetadata::default()
        })
    }
}

#[async_trait]
impl ToolExecutor for CompactContextTool {
    fn name(&self) -> &'static str {
        "compact_context"
    }

    fn schema(&self) -> ToolDefinition {
        ToolDefinition {
            name: "compact_context".to_string(),
            description: "Checkpoint your context before your next step: the older conversation \
                is summarized into a handoff and removed from your context, and the most recent \
                messages stay verbatim. Nothing is lost; `context_archive` can read any removed \
                message back. Mermaid also compacts on its own when the context window is nearly \
                full."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "focus": {
                        "type": "string",
                        "description": "What the handoff should emphasize."
                    }
                }
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, _ctx: ExecContext) -> ToolOutcome {
        let focus = str_arg(&args, "focus").map(str::to_string);
        ToolOutcome::success(
            "Checkpoint requested; it runs before your next step.",
            "requested",
            0.0,
        )
        .with_metadata(ToolRunMetadata {
            detail: ToolMetadata::CompactionRequest { focus },
            ..ToolRunMetadata::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ctx::test_exec_context;
    use mermaid_domain::{CompactionEvent, CompactionTrigger, ToolCallId, TurnId};

    fn record(id: &str) -> CompactionEvent {
        CompactionEvent {
            id: id.to_string(),
            trigger: CompactionTrigger::Manual,
            created_at: chrono::Local::now(),
            before_tokens: 100,
            after_tokens: 10,
            archived_message_count: 2,
            preserved_message_count: 1,
            preserved_turn_count: 1,
            summary_tokens: 5,
            duration_secs: 0.1,
            focus: None,
            archive_path: None,
        }
    }

    fn log() -> Vec<SessionEvent> {
        let kept = ChatMessage::user("third: carry on");
        let mut call = ChatMessage::assistant("writing it");
        call.tool_calls = Some(vec![mermaid_model::models::tool_call::ToolCall {
            id: Some("c1".to_string()),
            function: mermaid_model::models::tool_call::FunctionCall {
                name: "write_file".to_string(),
                arguments: serde_json::json!({"path": "src/lexer.rs"}),
            },
        }]);
        vec![
            SessionEvent::Message {
                message: ChatMessage::user("first: build the Lexer"),
            },
            SessionEvent::Message { message: call },
            SessionEvent::Message {
                message: ChatMessage::tool("c1", "write_file", "x".repeat(50_000)),
            },
            SessionEvent::Message {
                message: kept.clone(),
            },
            SessionEvent::Compaction {
                at: chrono::Local::now(),
                record: record("compact_1"),
                replacement: vec![ChatMessage::user("checkpoint"), kept],
            },
            SessionEvent::Message {
                message: ChatMessage::assistant("after the checkpoint"),
            },
        ]
    }

    #[test]
    fn messages_are_numbered_in_commit_order_and_marked_when_compacted() {
        let messages = archived_messages(log());
        assert_eq!(messages.len(), 5);
        assert_eq!(messages[0].number, 1);
        assert_eq!(messages[0].compacted_by.as_deref(), Some("compact_1"));
        assert_eq!(messages[2].compacted_by.as_deref(), Some("compact_1"));
        assert_eq!(messages[3].compacted_by, None, "the preserved tail is live");
        assert_eq!(messages[4].compacted_by, None);
    }

    #[test]
    fn a_reset_contributes_only_messages_not_seen_yet() {
        let first = ChatMessage::user("seeded by the backfill");
        let second = ChatMessage::assistant("also seeded");
        let events = vec![
            SessionEvent::Reset {
                at: chrono::Local::now(),
                messages: vec![first.clone(), second.clone()],
            },
            SessionEvent::Message {
                message: ChatMessage::user("appended later"),
            },
            // A rewind back to the first message: nothing new, nothing lost.
            SessionEvent::Reset {
                at: chrono::Local::now(),
                messages: vec![first],
            },
        ];
        let messages = archived_messages(events);
        let texts: Vec<&str> = messages
            .iter()
            .map(|m| m.message.content.as_str())
            .collect();
        assert_eq!(
            texts,
            ["seeded by the backfill", "also seeded", "appended later"]
        );
    }

    #[test]
    fn search_is_case_insensitive_and_reaches_tool_call_arguments() {
        let messages = archived_messages(log());
        let (out, matched) = search(&messages, "lexer", 1, 20);
        assert_eq!(matched, 2, "{out}");
        assert!(out.contains("#1 user [compacted by compact_1]"), "{out}");
        assert!(out.contains("#2 assistant"), "{out}");
        let (none, zero) = search(&messages, "parser", 1, 20);
        assert_eq!(zero, 0);
        assert!(none.contains("No messages match"));
    }

    #[test]
    fn search_pages_past_the_limit() {
        let messages = archived_messages(log());
        let (out, matched) = search(&messages, "e", 1, 1);
        assert!(matched > 1);
        assert!(out.contains("pass from="), "{out}");
    }

    #[test]
    fn read_returns_a_long_message_whole_across_pages() {
        let messages = archived_messages(log());
        let first = read(&messages, 3, 0).expect("message 3");
        assert!(first.contains("50000 chars"), "{}", &first[..120]);
        assert!(first.contains("char_offset=32000"));
        let rest = read(&messages, 3, 32_000).expect("rest");
        let body = rest.split_once("\n\n").expect("header").1;
        assert_eq!(body.len(), 18_000);
        assert!(read(&messages, 99, 0).is_err());
    }

    #[test]
    fn list_previews_every_message() {
        let messages = archived_messages(log());
        let (out, shown) = list(&messages, 1, 20);
        assert_eq!(shown, 5);
        assert!(out.contains("#5 assistant: after the checkpoint"), "{out}");
    }

    #[tokio::test]
    async fn archive_reads_the_session_log_from_disk() {
        let dir =
            std::env::temp_dir().join(format!("mermaid-context-archive-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let manager = crate::session::ConversationManager::new(&dir).expect("manager");
        let mut history = mermaid_domain::ConversationHistory::new(
            dir.display().to_string(),
            "ollama/test".to_string(),
            chrono::Local::now(),
        );
        history.add_messages(
            &[ChatMessage::user("remember the Zebra flag")],
            chrono::Local::now(),
        );
        let events = vec![SessionEvent::Message {
            message: history.messages()[0].clone(),
        }];
        manager
            .append_session_events(&history, &events)
            .expect("append");

        let (mut ctx, _progress) = test_exec_context(TurnId(1), ToolCallId(1), dir.clone());
        ctx.session_id = Some(history.id.clone());
        let outcome = ContextArchiveTool
            .execute(serde_json::json!({"query": "zebra"}), ctx)
            .await;
        assert!(outcome.is_success(), "{}", outcome.output());
        assert!(
            outcome.output().contains("Zebra flag"),
            "{}",
            outcome.output()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn archive_without_a_session_explains_itself() {
        let (ctx, _progress) = test_exec_context(TurnId(1), ToolCallId(1), "/tmp".into());
        let outcome = ContextArchiveTool
            .execute(serde_json::json!({"query": "x"}), ctx)
            .await;
        assert!(!outcome.is_success());
        assert!(outcome.output().contains("no session log"));
    }

    #[tokio::test]
    async fn compact_context_records_the_request_with_its_focus() {
        let (ctx, _progress) = test_exec_context(TurnId(1), ToolCallId(1), "/tmp".into());
        let outcome = CompactContextTool
            .execute(serde_json::json!({"focus": "the parser"}), ctx)
            .await;
        assert!(outcome.is_success());
        assert_eq!(
            outcome.metadata.detail,
            ToolMetadata::CompactionRequest {
                focus: Some("the parser".to_string())
            }
        );
    }
}
