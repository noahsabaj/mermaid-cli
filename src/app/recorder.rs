//! `--record` / `--replay` support.
//!
//! The Elm/MVU architecture makes deterministic replay nearly free:
//! if you capture every `Msg` the reducer sees, you can reconstruct
//! the exact final `State` by folding over that log. This module
//! implements both sides.
//!
//! Wire format: one JSON object per line (JSONL). Each object is
//! `{ts, kind, body}`:
//!   - `ts`: RFC3339 timestamp (for debugging, not replay).
//!   - `kind`: `MsgKind` variant tag (matches `Msg::kind().into()`).
//!   - `body`: best-effort structured payload from `record_msg_body`.
//!
//! Not every `Msg` field is safely serializable today — raw image
//! bytes in `Paste::Image`, for example. Unsupported payloads are
//! marked with `"recordable": false` and compact metadata. Replay is
//! a best-effort reconstruction.
//!
//! For C6 this ships the on-disk shape + a `Recorder` type that
//! writes; replay reading is available but opt-in (serialize
//! support is wired on a subset of Msg variants that don't carry
//! binary payloads). C9 rounds out coverage with the parity
//! harness.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Local;

use crate::domain::{KeyCode, Msg, MsgKind, Paste, SlashCmd, ToolOutcome, TurnId};
use crate::providers::{ProgressEvent, SubagentPhase};

/// Append-only recorder. Writes one JSONL line per `Msg` the main
/// loop chooses to log.
pub struct Recorder {
    writer: BufWriter<File>,
    path: PathBuf,
}

impl Recorder {
    /// Open `path` for append. Creates the file if it doesn't exist.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("open {} for recording", path.display()))?;
        Ok(Self {
            writer: BufWriter::new(file),
            path,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Record a single `MsgKind` + optional body JSON. Meant for the
    /// narrow subset of variants that survive round-trip. Full
    /// Msg-graph coverage comes in C9.
    pub fn record_kind(
        &mut self,
        kind: MsgKind,
        turn: Option<TurnId>,
        body: serde_json::Value,
    ) -> Result<()> {
        let entry = serde_json::json!({
            "ts": Local::now().to_rfc3339(),
            "kind": format!("{:?}", kind),
            "turn": turn.map(|t| t.0),
            "body": body,
        });
        writeln!(self.writer, "{}", entry).context("write jsonl line")?;
        Ok(())
    }

    pub fn flush(&mut self) -> Result<()> {
        self.writer.flush().context("flush recorder")
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        let _ = self.writer.flush();
    }
}

/// Compact, best-effort JSON body for a reducer `Msg`.
///
/// This deliberately records useful payloads without claiming the full
/// `Msg` graph can round-trip through serde today. Runtime-only or
/// binary-heavy variants are marked explicitly so replay tooling can
/// decide how to handle them.
pub fn record_msg_body(msg: &Msg) -> serde_json::Value {
    match msg {
        Msg::Key(key) => serde_json::json!({
            "code": key_code_body(key.code),
            "modifiers": {
                "ctrl": key.modifiers.ctrl,
                "alt": key.modifiers.alt,
                "shift": key.modifiers.shift,
            },
        }),
        Msg::Paste(Paste::Text(text)) => serde_json::json!({
            "type": "text",
            "text": text,
        }),
        Msg::Paste(Paste::Image { bytes, format }) => serde_json::json!({
            "recordable": false,
            "reason": "binary paste image omitted",
            "type": "image",
            "format": format,
            "size_bytes": bytes.len(),
        }),
        Msg::SubmitPrompt {
            text,
            attachment_ids,
        } => serde_json::json!({
            "text": text,
            "attachment_ids": attachment_ids,
        }),
        Msg::Slash(cmd) => slash_body(cmd),
        Msg::CancelTurn => serde_json::json!({}),
        Msg::ConfirmAccepted => serde_json::json!({"accepted": true}),
        Msg::ConfirmDeclined => serde_json::json!({"accepted": false}),
        Msg::Quit => serde_json::json!({}),
        Msg::RuntimeSignal(signal) => serde_json::json!({
            "signal": signal.as_str(),
        }),
        Msg::StreamText { chunk, .. } => serde_json::json!({"chunk": chunk}),
        Msg::StreamReasoning { chunk, .. } => serde_json::json!({
            "text": chunk.text,
            "signature": chunk.signature,
        }),
        Msg::StreamToolCall { call, .. } => serde_json::to_value(call)
            .unwrap_or_else(|_| unsupported("tool call was not serializable")),
        Msg::ContextUsageEstimated { snapshot, .. } => serde_json::json!({
            "used_tokens": snapshot.used_tokens,
            "max_tokens": snapshot.max_tokens,
            "remaining_tokens": snapshot.remaining_tokens,
            "used_percent": snapshot.used_percent,
            "source": format!("{:?}", snapshot.source),
            "breakdown": snapshot.breakdown.as_ref().map(|b| serde_json::json!({
                "system_tokens": b.system_tokens,
                "instructions_tokens": b.instructions_tokens,
                "message_tokens": b.message_tokens,
                "tool_schema_tokens": b.tool_schema_tokens,
                "image_count": b.image_count,
                "message_count": b.message_count,
                "tool_count": b.tool_count,
            })),
        }),
        Msg::CompactionFinished { result, .. } => serde_json::json!({
            "id": result.record.id,
            "trigger": result.record.trigger.as_str(),
            "before_tokens": result.record.before_tokens,
            "after_tokens": result.record.after_tokens,
            "archived_message_count": result.record.archived_message_count,
            "preserved_message_count": result.record.preserved_message_count,
            "duration_secs": result.record.duration_secs,
        }),
        Msg::CompactionFailed {
            trigger,
            message,
            kind,
            ..
        } => serde_json::json!({
            "trigger": trigger.as_str(),
            "message": message,
            "kind": format!("{:?}", kind),
        }),
        Msg::StreamDone {
            usage,
            thinking_signature,
            ..
        } => serde_json::json!({
            "usage": usage.as_ref().map(|u| serde_json::json!({
                "prompt_tokens": u.prompt_tokens,
                "completion_tokens": u.completion_tokens,
                "total_tokens": u.total_tokens,
                "cached_input_tokens": u.cached_input_tokens,
                "cache_creation_input_tokens": u.cache_creation_input_tokens,
                "reasoning_output_tokens": u.reasoning_output_tokens,
                "source": format!("{:?}", u.source),
            })),
            "thinking_signature": thinking_signature,
        }),
        Msg::UpstreamError { error, .. } => serde_json::to_value(error)
            .unwrap_or_else(|_| unsupported("upstream error was not serializable")),
        Msg::TurnCancelled(_) => serde_json::json!({}),
        Msg::ToolStarted { call_id, .. } => serde_json::json!({"call_id": call_id.0}),
        Msg::ToolProgress { call_id, event, .. } => serde_json::json!({
            "call_id": call_id.0,
            "event": progress_body(event),
        }),
        Msg::ToolFinished {
            call_id, outcome, ..
        } => serde_json::json!({
            "call_id": call_id.0,
            "outcome": outcome_body(outcome),
        }),
        Msg::ApprovalRequested {
            call_id,
            tool,
            risk,
            ..
        } => serde_json::json!({
            "call_id": call_id.0,
            "tool": tool,
            "risk": risk,
        }),
        Msg::McpServerReady { name, tools } => serde_json::json!({
            "name": name,
            "tools": tools.iter().map(|tool| serde_json::json!({
                "name": tool.name,
                "description": tool.description,
                "input_schema": tool.input_schema,
            })).collect::<Vec<_>>(),
        }),
        Msg::McpServerErrored { name, reason } => serde_json::json!({
            "name": name,
            "reason": reason,
        }),
        Msg::McpServerStopped { name } => serde_json::json!({"name": name}),
        Msg::InstructionsChanged(loaded) => match loaded {
            Some(loaded) => serde_json::json!({
                "path": loaded.path,
                "byte_len": loaded.byte_len,
                "truncated": loaded.truncated,
            }),
            None => serde_json::json!({"path": null}),
        },
        Msg::SessionSaved => serde_json::json!({}),
        Msg::ConversationLoaded(history) => serde_json::json!({
            "id": history.id,
            "message_count": history.messages.len(),
            "title": history.title,
        }),
        Msg::ConversationsListed(summaries) => serde_json::json!({
            "count": summaries.len(),
            "ids": summaries.iter().map(|summary| summary.id.as_str()).collect::<Vec<_>>(),
        }),
        Msg::RuntimeTasksListed(tasks) => serde_json::json!({
            "count": tasks.len(),
            "ids": tasks.iter().map(|task| task.id.as_str()).collect::<Vec<_>>(),
        }),
        Msg::RuntimeTaskLoaded { task, events } => serde_json::json!({
            "id": task.as_ref().map(|task| task.id.as_str()),
            "found": task.is_some(),
            "event_count": events.len(),
        }),
        Msg::RuntimeProcessesListed(processes) => serde_json::json!({
            "count": processes.len(),
            "ids": processes.iter().map(|process| process.id.as_str()).collect::<Vec<_>>(),
        }),
        Msg::RuntimeText(text) => serde_json::json!({"chars": text.len()}),
        Msg::RuntimeApprovalsListed(approvals) => serde_json::json!({
            "count": approvals.len(),
            "ids": approvals.iter().map(|approval| approval.id.as_str()).collect::<Vec<_>>(),
        }),
        Msg::RuntimeCheckpointsListed(checkpoints) => serde_json::json!({
            "count": checkpoints.len(),
            "ids": checkpoints.iter().map(|checkpoint| checkpoint.id.as_str()).collect::<Vec<_>>(),
        }),
        Msg::RuntimeMemoryListed(memory) => serde_json::json!({
            "count": memory.len(),
            "ids": memory.iter().map(|entry| entry.id.as_str()).collect::<Vec<_>>(),
        }),
        Msg::RuntimePluginsListed(plugins) => serde_json::json!({
            "count": plugins.len(),
            "ids": plugins.iter().map(|plugin| plugin.id.as_str()).collect::<Vec<_>>(),
        }),
        Msg::ModelPullFinished { model } => serde_json::json!({"model": model}),
        Msg::ModelPullProgress(line) => serde_json::json!({"line": line}),
        Msg::Tick => serde_json::json!({}),
        Msg::StatusDismiss => serde_json::json!({}),
        Msg::Resize { width, height } => serde_json::json!({
            "width": width,
            "height": height,
        }),
        Msg::TransientStatus {
            text,
            kind,
            dismiss_ms,
        } => serde_json::json!({
            "text": text,
            "kind": format!("{:?}", kind),
            "dismiss_ms": dismiss_ms,
        }),
        Msg::MouseScroll { delta } => serde_json::json!({"delta": delta}),
        Msg::OpenImageAt {
            message_index,
            image_index,
        } => serde_json::json!({
            "message_index": message_index,
            "image_index": image_index,
        }),
    }
}

fn key_code_body(code: KeyCode) -> serde_json::Value {
    match code {
        KeyCode::Char(c) => serde_json::json!({"char": c.to_string()}),
        KeyCode::F(n) => serde_json::json!({"f": n}),
        other => serde_json::json!(format!("{:?}", other)),
    }
}

fn slash_body(cmd: &SlashCmd) -> serde_json::Value {
    match cmd {
        SlashCmd::Model(model) => serde_json::json!({"command": "model", "arg": model}),
        SlashCmd::Reasoning(level) => serde_json::json!({
            "command": "reasoning",
            "arg": level.map(|level| level.as_str()),
        }),
        SlashCmd::VisibleReasoning(arg) => {
            serde_json::json!({"command": "visible-reasoning", "arg": arg})
        },
        SlashCmd::Safety(mode) => serde_json::json!({
            "command": "safety",
            "arg": mode.map(|m| m.as_str()),
        }),
        SlashCmd::Clear => serde_json::json!({"command": "clear"}),
        SlashCmd::Save(name) => serde_json::json!({"command": "save", "arg": name}),
        SlashCmd::Load(name) => serde_json::json!({"command": "load", "arg": name}),
        SlashCmd::List => serde_json::json!({"command": "list"}),
        SlashCmd::Usage => serde_json::json!({"command": "usage"}),
        SlashCmd::Context => serde_json::json!({"command": "context"}),
        SlashCmd::Compact(instructions) => {
            serde_json::json!({"command": "compact", "arg": instructions})
        },
        SlashCmd::Doctor => serde_json::json!({"command": "doctor"}),
        SlashCmd::Tasks => serde_json::json!({"command": "tasks"}),
        SlashCmd::Task(id) => serde_json::json!({"command": "task", "arg": id}),
        SlashCmd::Pause(id) => serde_json::json!({"command": "pause", "arg": id}),
        SlashCmd::Resume(id) => serde_json::json!({"command": "resume", "arg": id}),
        SlashCmd::Cancel(id) => serde_json::json!({"command": "cancel", "arg": id}),
        SlashCmd::Handoff(id) => serde_json::json!({"command": "handoff", "arg": id}),
        SlashCmd::Report(id) => serde_json::json!({"command": "report", "arg": id}),
        SlashCmd::Processes => serde_json::json!({"command": "processes"}),
        SlashCmd::Logs(id) => serde_json::json!({"command": "logs", "arg": id}),
        SlashCmd::Stop(id) => serde_json::json!({"command": "stop", "arg": id}),
        SlashCmd::Restart(id) => serde_json::json!({"command": "restart", "arg": id}),
        SlashCmd::Open(target) => serde_json::json!({"command": "open", "arg": target}),
        SlashCmd::Ports => serde_json::json!({"command": "ports"}),
        SlashCmd::Approvals => serde_json::json!({"command": "approvals"}),
        SlashCmd::Approve(id) => serde_json::json!({"command": "approve", "arg": id}),
        SlashCmd::Deny(id) => serde_json::json!({"command": "deny", "arg": id}),
        SlashCmd::Checkpoint(paths) => serde_json::json!({"command": "checkpoint", "arg": paths}),
        SlashCmd::Checkpoints => serde_json::json!({"command": "checkpoints"}),
        SlashCmd::Restore(id) => serde_json::json!({"command": "restore", "arg": id}),
        SlashCmd::Memory => serde_json::json!({"command": "memory"}),
        SlashCmd::MemoryEdit(input) => serde_json::json!({"command": "memory edit", "arg": input}),
        SlashCmd::Remember(entry) => serde_json::json!({"command": "remember", "arg": entry}),
        SlashCmd::Forget(id) => serde_json::json!({"command": "forget", "arg": id}),
        SlashCmd::ModelInfo(model) => serde_json::json!({"command": "model-info", "arg": model}),
        SlashCmd::Plugins => serde_json::json!({"command": "plugins"}),
        SlashCmd::CloudSetup => serde_json::json!({"command": "cloud-setup"}),
        SlashCmd::Help => serde_json::json!({"command": "help"}),
        SlashCmd::Quit => serde_json::json!({"command": "quit"}),
        SlashCmd::Unknown(name) => serde_json::json!({"command": "unknown", "name": name}),
    }
}

fn progress_body(event: &ProgressEvent) -> serde_json::Value {
    match event {
        ProgressEvent::Output(text) => serde_json::json!({"type": "output", "text": text}),
        ProgressEvent::Status(text) => serde_json::json!({"type": "status", "text": text}),
        ProgressEvent::Bytes { done, total } => serde_json::json!({
            "type": "bytes",
            "done": done,
            "total": total,
        }),
        ProgressEvent::Artifact {
            mime,
            data,
            caption,
        } => serde_json::json!({
            "recordable": false,
            "type": "artifact",
            "mime": mime,
            "caption": caption,
            "size_bytes": data.len(),
        }),
        ProgressEvent::SubagentToolCall {
            child_call_id,
            tool_name,
            phase,
        } => serde_json::json!({
            "type": "subagent_tool_call",
            "child_call_id": child_call_id.0,
            "tool_name": tool_name,
            "phase": subagent_phase(*phase),
        }),
        ProgressEvent::SubagentText(text) => {
            serde_json::json!({"type": "subagent_text", "text": text})
        },
    }
}

fn subagent_phase(phase: SubagentPhase) -> &'static str {
    match phase {
        SubagentPhase::Started => "started",
        SubagentPhase::Finished => "finished",
        SubagentPhase::Errored => "errored",
    }
}

fn outcome_body(outcome: &ToolOutcome) -> serde_json::Value {
    serde_json::json!({
        "status": match outcome.status {
            crate::domain::ToolStatus::Success => "success",
            crate::domain::ToolStatus::Error => "error",
            crate::domain::ToolStatus::Cancelled => "cancelled",
        },
        "summary": outcome.summary,
        "model_content": outcome.model_content,
        "error": outcome.error,
        "image_count": outcome.images().map(|images| images.len()).unwrap_or(0),
        "duration_secs": outcome.duration_secs,
        "metadata": outcome.metadata,
        "artifacts": outcome.artifacts,
    })
}

fn unsupported(reason: &str) -> serde_json::Value {
    serde_json::json!({
        "recordable": false,
        "reason": reason,
    })
}

/// Read a JSONL log back. Iterates one line at a time so a huge
/// replay doesn't allocate the whole file upfront.
pub struct Replay {
    lines: std::io::Lines<BufReader<File>>,
    path: PathBuf,
}

impl Replay {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let file =
            File::open(&path).with_context(|| format!("open {} for replay", path.display()))?;
        Ok(Self {
            lines: BufReader::new(file).lines(),
            path,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Iterator for Replay {
    type Item = Result<ReplayEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        let line = self.lines.next()?;
        Some(match line {
            Ok(raw) => serde_json::from_str::<ReplayEntry>(&raw)
                .with_context(|| format!("parse replay line: {}", raw)),
            Err(e) => Err(anyhow::Error::from(e)),
        })
    }
}

/// Parsed JSONL entry. Fields mirror what `Recorder::record_kind`
/// writes.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct ReplayEntry {
    pub ts: String,
    pub kind: String,
    pub turn: Option<u64>,
    pub body: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpfile(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("mermaid_recorder_tests");
        let _ = std::fs::create_dir_all(&dir);
        dir.join(name)
    }

    #[test]
    fn record_and_replay_roundtrip() {
        let path = tmpfile("roundtrip.jsonl");
        let _ = std::fs::remove_file(&path);

        {
            let mut r = Recorder::open(&path).expect("open");
            r.record_kind(MsgKind::Tick, None, serde_json::json!({}))
                .expect("record");
            r.record_kind(
                MsgKind::SubmitPrompt,
                None,
                serde_json::json!({"text": "hello"}),
            )
            .expect("record");
            r.record_kind(
                MsgKind::StreamText,
                Some(TurnId(7)),
                serde_json::json!({"chunk": "partial"}),
            )
            .expect("record");
            r.flush().expect("flush");
        }

        let replay = Replay::open(&path).expect("open replay");
        let entries: Vec<_> = replay.collect::<Result<_>>().expect("all parse");
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].kind, "Tick");
        assert_eq!(entries[1].body["text"], "hello");
        assert_eq!(entries[2].turn, Some(7));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn replay_parses_malformed_line_as_err() {
        let path = tmpfile("bad.jsonl");
        std::fs::write(&path, "not-json\n").expect("write");
        let mut replay = Replay::open(&path).expect("open");
        let first = replay.next().expect("first entry");
        assert!(first.is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn record_creates_file_on_open() {
        let path = tmpfile("creates.jsonl");
        let _ = std::fs::remove_file(&path);
        assert!(!path.exists());
        let _ = Recorder::open(&path).expect("open");
        assert!(path.exists());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn record_append_preserves_existing_lines() {
        let path = tmpfile("append.jsonl");
        let _ = std::fs::remove_file(&path);
        {
            let mut r = Recorder::open(&path).expect("open");
            r.record_kind(MsgKind::Tick, None, serde_json::json!({}))
                .expect("record");
        }
        {
            let mut r = Recorder::open(&path).expect("reopen");
            r.record_kind(MsgKind::Quit, None, serde_json::json!({}))
                .expect("record");
        }
        let replay = Replay::open(&path).expect("replay");
        let entries: Vec<_> = replay.collect::<Result<_>>().expect("all parse");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].kind, "Tick");
        assert_eq!(entries[1].kind, "Quit");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn record_msg_body_submit_prompt_keeps_text_and_attachments() {
        let body = record_msg_body(&crate::domain::Msg::SubmitPrompt {
            text: "explain main.rs".to_string(),
            attachment_ids: vec![3, 9],
        });
        assert_eq!(body["text"], "explain main.rs");
        assert_eq!(body["attachment_ids"][0], 3);
        assert_eq!(body["attachment_ids"][1], 9);
    }

    #[test]
    fn record_msg_body_slash_model_keeps_command_and_arg() {
        let body = record_msg_body(&crate::domain::Msg::Slash(crate::domain::SlashCmd::Model(
            Some("anthropic/opus".to_string()),
        )));
        assert_eq!(body["command"], "model");
        assert_eq!(body["arg"], "anthropic/opus");
    }

    #[test]
    fn record_msg_body_runtime_signal_keeps_signal_name() {
        let body = record_msg_body(&crate::domain::Msg::RuntimeSignal(
            crate::domain::RuntimeSignal::Terminate,
        ));
        assert_eq!(body["signal"], "terminate");
    }

    #[test]
    fn record_msg_body_marks_binary_paste_image_unrecordable() {
        let body = record_msg_body(&crate::domain::Msg::Paste(crate::domain::Paste::Image {
            bytes: vec![1, 2, 3],
            format: "png".to_string(),
        }));
        assert_eq!(body["recordable"], false);
        assert_eq!(body["type"], "image");
        assert_eq!(body["size_bytes"], 3);
    }
}
