//! `--record` / `--replay` support.
//!
//! The Elm/MVU architecture makes deterministic replay nearly free:
//! `update(State, Msg)` is a pure function of its inputs (the wall clock is
//! injected as `state.now`), so capturing every `Msg` the reducer sees —
//! plus the clock value it was reduced under — lets `--replay` reconstruct
//! the exact final `State` by folding over the log.
//!
//! Wire format (version 1), one JSON object per line (JSONL):
//!   - Line 1 — [`SessionHeader`]: everything replay needs to rebuild the
//!     initial `State` (format version, startup clock, model, cwd, the full
//!     `Config`, and any `--continue` seed conversation). Self-contained: a
//!     recording replays without reading the machine's live config.
//!   - Every later line — [`ReplayEntry`] `{ts, kind, turn, msg}`:
//!     - `ts`: the exact `state.now` the reducer saw for this input. The
//!       driver stamps one clock per tick and shares it between the
//!       recording and the reducer, so replay reproduces the fold bit-exactly.
//!     - `kind` / `turn`: denormalized copies of `Msg::kind()` /
//!       `Msg::turn_id()` for grepping a log by hand; replay reads `msg`.
//!     - `msg`: the full `Msg`, serde-serialized (externally tagged). Binary
//!       payloads (pasted images, tool artifacts) ride as base64 and replay
//!       bit-exactly; new `Msg` variants round-trip automatically.
//!
//! Two deliberate divergences from live state, both security-driven:
//!   - Credential-shaped strings are redacted before hitting disk (#17), so
//!     a session where a secret crossed the reducer replays the *redacted*
//!     transcript. Replay is deterministic with respect to the log — folding
//!     the same log twice always produces identical state — and identical to
//!     the live session whenever no redaction fired.
//!   - The copied-selection payload (`Msg::CopySelection`) is recorded as a
//!     placeholder: the text is already in the transcript, can be huge, and
//!     the reducer ignores it (the payload only feeds a clipboard `Cmd`).

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

use crate::domain::Config;
use crate::domain::ConversationHistory;
use crate::domain::{Msg, Session};

/// Bumped when the wire shape changes incompatibly. Replay refuses logs
/// written by a different version rather than folding garbage.
pub const RECORDING_FORMAT_VERSION: u32 = 1;

/// First line of every recording: everything `--replay` needs to rebuild the
/// session's initial `State` without touching the live machine's config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionHeader {
    pub format: u32,
    /// Startup clock — seeds `State::new`'s injected `now`, which derives the
    /// initial conversation id/title.
    pub ts: DateTime<Local>,
    pub model_id: String,
    pub cwd: PathBuf,
    /// Full config snapshot (redacted). `State::new` derives MCP seeding and
    /// per-model reasoning from it.
    pub config: Config,
    /// `--continue` / `--sessions` seed applied before the first frame.
    #[serde(default)]
    pub seed_conversation: Option<ConversationHistory>,
}

/// Append-only recorder. Writes the session header once, then one JSONL
/// line per `Msg` the main loop feeds the reducer.
pub struct Recorder {
    writer: BufWriter<File>,
}

impl Recorder {
    /// Open `path` for append. Creates the file if it doesn't exist.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let mut opts = OpenOptions::new();
        opts.create(true).append(true);
        // A recording stores the full conversation — prompts, model output, and
        // tool results (e.g. a `read_file` of a private doc) — in cleartext;
        // only credential-shaped strings are scrubbed. Create it owner-only so a
        // shared temp/cwd doesn't leak it (#132).
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let file = opts
            .open(&path)
            .with_context(|| format!("open {} for recording", path.display()))?;
        // `mode` only applies on create; tighten an existing recording too.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        tracing::warn!(
            path = %path.display(),
            "session recording is ON: this file stores prompts, model output, and \
             tool results (including file contents) in cleartext; only \
             credential-shaped strings are redacted",
        );
        Ok(Self {
            writer: BufWriter::new(file),
        })
    }

    /// Write the session header. Called once, before the first `record_msg`,
    /// and flushed immediately — a replay of a crashed session should still
    /// find a parseable header.
    pub fn record_header(&mut self, header: &SessionHeader) -> Result<()> {
        let mut value = serde_json::to_value(header).context("serialize session header")?;
        mermaid_model::utils::redact_json(&mut value);
        writeln!(self.writer, "{}", value).context("write header line")?;
        self.flush()
    }

    /// Record one reducer input. `now` must be the same value the driver
    /// stamps into `state.now` for this tick — that identity is what makes
    /// the recorded `ts` a faithful replay clock.
    ///
    /// `Msg::Tick` is deliberately NOT recorded: the reducer's Tick arm is a
    /// documented no-op (render derives the spinner from `state.now`), so a
    /// 60 Hz tick stream would bloat the log by megabytes per hour for zero
    /// replay fidelity. The `tick_is_a_reducer_noop` invariant test pins
    /// this — if Tick ever grows state effects, that test fails and ticks
    /// must be recorded again. Old logs containing Tick entries still fold
    /// fine (a no-op replays as a no-op).
    pub fn record_msg(&mut self, now: DateTime<Local>, msg: &Msg) -> Result<()> {
        if matches!(msg, Msg::Tick) {
            return Ok(());
        }
        // The copied selection is already visible in the transcript and the
        // reducer never reads the payload (it only feeds `Cmd::CopyToClipboard`),
        // so persist a placeholder instead of duplicating potentially-huge text.
        let sanitized;
        let msg = match msg {
            Msg::CopySelection(text) => {
                sanitized = Msg::CopySelection(format!("[{} chars]", text.chars().count()));
                &sanitized
            },
            other => other,
        };
        let mut body = serde_json::to_value(msg).context("serialize msg")?;
        // Single redaction choke point: scrub credential-shaped strings out of
        // every recorded payload before it hits disk. A `read_file .env` result,
        // a pasted token, or an API error echoing a key would otherwise be
        // persisted in cleartext in the `--record` log (#17).
        mermaid_model::utils::redact_json(&mut body);
        let entry = serde_json::json!({
            "ts": now,
            "kind": format!("{:?}", msg.kind()),
            "turn": msg.turn_id().map(|t| t.0),
            "msg": body,
        });
        writeln!(self.writer, "{}", entry).context("write jsonl line")?;
        Ok(())
    }

    /// Seal the recording with a fingerprint of the final session state, so
    /// a future `--replay` can verify its fold reproduces what this live
    /// session actually saw (not merely that the fold is self-consistent).
    /// Written on clean exit; a crashed session simply has no trailer.
    pub fn record_trailer(&mut self, now: DateTime<Local>, session: &Session) -> Result<()> {
        let trailer = SessionTrailer {
            ts: now,
            final_session_fingerprint: session_fingerprint(session),
        };
        let line = serde_json::to_string(&trailer).context("serialize session trailer")?;
        writeln!(self.writer, "{}", line).context("write trailer line")?;
        self.flush()
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

/// Stable fingerprint of the session outcome — the durable, user-visible
/// half of `State`: the full conversation (messages, ids, titles,
/// timestamps), model id, reasoning/safety modes, and token accounting.
///
/// Deliberately hashes ONLY `state.session`, excluding machine-derived
/// fields (`temp_dir`) and `settings` (whose recorded copy is redacted), so
/// the same log folds to the same fingerprint on any machine. A mismatch
/// against a recorded trailer therefore means exactly one thing: the fold
/// no longer reproduces the live session's outcome — expected when
/// redaction fired mid-session or the reducer changed since recording,
/// alarming otherwise.
pub fn session_fingerprint(session: &Session) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    let mut hasher = Sha256::new();
    hasher.update(format!("{session:?}").as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity("sha256:".len() + digest.len() * 2);
    out.push_str("sha256:");
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Final line of a cleanly-exited recording: the fingerprint `--replay`
/// verifies its fold against.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTrailer {
    pub ts: DateTime<Local>,
    pub final_session_fingerprint: String,
}

/// Parsed JSONL entry. Fields mirror what [`Recorder::record_msg`] writes.
#[derive(Debug, Serialize, Deserialize)]
pub struct ReplayEntry {
    pub ts: DateTime<Local>,
    pub kind: String,
    pub turn: Option<u64>,
    pub msg: serde_json::Value,
}

impl ReplayEntry {
    /// Reconstruct the reducer input. The error names the recorded `kind` so
    /// a replay report can say what it skipped (e.g. a variant this build
    /// doesn't know because the log came from a newer mermaid).
    pub fn to_msg(&self) -> Result<Msg> {
        serde_json::from_value(self.msg.clone())
            .with_context(|| format!("reconstruct recorded {} msg", self.kind))
    }
}

/// One classified line of a recording, after the leading header.
#[derive(Debug)]
pub enum RecordLine {
    /// A normal `{ts, kind, turn, msg}` entry.
    Entry(ReplayEntry),
    /// The clean-exit seal: a fingerprint of the live session's final state.
    Trailer(SessionTrailer),
    /// Another session header: `Recorder::open` appends, so a reused
    /// `--record` path holds multiple sessions back to back. Replay folds
    /// the first session and stops here.
    Header(Box<SessionHeader>),
    /// Neither an entry, trailer, nor header — corrupt or truncated write.
    Malformed { raw: String, error: String },
}

/// Read a recording back. Yields one classified [`RecordLine`] at a time so
/// a huge log doesn't allocate the whole file upfront.
#[derive(Debug)]
pub struct Replay {
    lines: std::io::Lines<BufReader<File>>,
}

impl Replay {
    /// Open a recording and parse its leading [`SessionHeader`]. Refuses
    /// files without one (pre-v1 logs) and format versions this build
    /// doesn't understand.
    pub fn open(path: impl Into<PathBuf>) -> Result<(SessionHeader, Self)> {
        let path = path.into();
        let file =
            File::open(&path).with_context(|| format!("open {} for replay", path.display()))?;
        let mut lines = BufReader::new(file).lines();
        let first = lines
            .next()
            .context("recording is empty — no session header")?
            .context("read session header line")?;
        let header: SessionHeader = serde_json::from_str(&first).context(
            "recording has no parseable session header — \
             was it written by an older mermaid or truncated at byte 0?",
        )?;
        anyhow::ensure!(
            header.format == RECORDING_FORMAT_VERSION,
            "recording format {} is not supported (this build reads format {})",
            header.format,
            RECORDING_FORMAT_VERSION,
        );
        Ok((header, Self { lines }))
    }
}

impl Iterator for Replay {
    type Item = std::io::Result<RecordLine>;

    fn next(&mut self) -> Option<Self::Item> {
        let raw = match self.lines.next()? {
            Ok(raw) => raw,
            Err(e) => return Some(Err(e)),
        };
        // Entries are the overwhelmingly common case; a trailer seals a
        // cleanly-exited session; a header mid-file marks the start of an
        // appended second session.
        let line = match serde_json::from_str::<ReplayEntry>(&raw) {
            Ok(entry) => RecordLine::Entry(entry),
            Err(entry_err) => match serde_json::from_str::<SessionTrailer>(&raw) {
                Ok(trailer) => RecordLine::Trailer(trailer),
                Err(_) => match serde_json::from_str::<SessionHeader>(&raw) {
                    Ok(header) => RecordLine::Header(Box::new(header)),
                    Err(_) => RecordLine::Malformed {
                        raw,
                        error: entry_err.to_string(),
                    },
                },
            },
        };
        Some(Ok(line))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ClipboardRead, MsgKind, Paste, TurnId};

    fn tmpfile(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("mermaid_recorder_tests");
        let _ = std::fs::create_dir_all(&dir);
        dir.join(name)
    }

    fn test_header(ts: DateTime<Local>) -> SessionHeader {
        SessionHeader {
            format: RECORDING_FORMAT_VERSION,
            ts,
            model_id: "ollama/test".to_string(),
            cwd: PathBuf::from("/tmp/project"),
            config: Config::default(),
            seed_conversation: None,
        }
    }

    fn fixed_ts() -> DateTime<Local> {
        // A fixed instant so assertions are stable.
        chrono::DateTime::parse_from_rfc3339("2026-07-02T12:00:00.123+00:00")
            .unwrap()
            .with_timezone(&Local)
    }

    #[cfg(unix)]
    #[test]
    fn recording_file_is_owner_only() {
        // #132: recordings hold cleartext prompts/output/file-contents, so they
        // must be created 0600 rather than inheriting a world-readable umask.
        use std::os::unix::fs::PermissionsExt;
        let path = tmpfile("perms.jsonl");
        let _ = std::fs::remove_file(&path);
        let _ = Recorder::open(&path).expect("open");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "recording must be created owner-only");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn record_and_replay_roundtrip() {
        let path = tmpfile("roundtrip.jsonl");
        let _ = std::fs::remove_file(&path);
        let ts = fixed_ts();

        {
            let mut r = Recorder::open(&path).expect("open");
            r.record_header(&test_header(ts)).expect("header");
            r.record_msg(ts, &Msg::SessionSaved).expect("record");
            r.record_msg(
                ts,
                &Msg::SubmitPrompt {
                    text: "hello".to_string(),
                    attachment_ids: vec![3, 9],
                },
            )
            .expect("record");
            r.record_msg(
                ts,
                &Msg::StreamText {
                    turn: TurnId(7),
                    chunk: "partial".to_string(),
                },
            )
            .expect("record");
            r.flush().expect("flush");
        }

        let (header, replay) = Replay::open(&path).expect("open replay");
        assert_eq!(header.model_id, "ollama/test");
        assert_eq!(header.ts, ts);

        let lines: Vec<_> = replay.collect::<std::io::Result<_>>().expect("read all");
        assert_eq!(lines.len(), 3);
        let entries: Vec<&ReplayEntry> = lines
            .iter()
            .map(|l| match l {
                RecordLine::Entry(e) => e,
                other => panic!("expected entry, got {other:?}"),
            })
            .collect();
        assert_eq!(entries[0].kind, "SessionSaved");
        assert!(matches!(entries[0].to_msg().unwrap(), Msg::SessionSaved));
        match entries[1].to_msg().unwrap() {
            Msg::SubmitPrompt {
                text,
                attachment_ids,
            } => {
                assert_eq!(text, "hello");
                assert_eq!(attachment_ids, vec![3, 9]);
            },
            other => panic!("expected SubmitPrompt, got {other:?}"),
        }
        assert_eq!(entries[2].turn, Some(7));
        assert_eq!(entries[2].ts, ts);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn record_msg_redacts_secrets_in_body() {
        // A recorded payload carrying a credential (e.g. a `read_file .env`
        // result or an API error echoing a key) must hit disk scrubbed (#17).
        let path = tmpfile("redact.jsonl");
        let _ = std::fs::remove_file(&path);
        {
            let mut r = Recorder::open(&path).expect("open");
            r.record_header(&test_header(fixed_ts())).expect("header");
            r.record_msg(
                fixed_ts(),
                &Msg::StreamText {
                    turn: TurnId(1),
                    chunk: "OPENAI_API_KEY=sk-abcdefghijklmnop1234".to_string(),
                },
            )
            .expect("record");
            r.flush().expect("flush");
        }

        let raw = std::fs::read_to_string(&path).expect("read back");
        assert!(
            !raw.contains("sk-abcdefghijklmnop1234"),
            "raw secret leaked: {raw}"
        );
        assert!(
            raw.contains("[REDACTED]"),
            "expected redaction marker: {raw}"
        );

        let (_, mut replay) = Replay::open(&path).expect("replay");
        let line = replay.next().expect("one line").expect("io ok");
        let RecordLine::Entry(entry) = line else {
            panic!("expected entry");
        };
        match entry.to_msg().unwrap() {
            Msg::StreamText { chunk, .. } => {
                assert_eq!(chunk, "OPENAI_API_KEY=[REDACTED]");
            },
            other => panic!("expected StreamText, got {other:?}"),
        }

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn copy_selection_is_recorded_as_placeholder() {
        let path = tmpfile("copysel.jsonl");
        let _ = std::fs::remove_file(&path);
        {
            let mut r = Recorder::open(&path).expect("open");
            r.record_header(&test_header(fixed_ts())).expect("header");
            r.record_msg(
                fixed_ts(),
                &Msg::CopySelection("secret transcript".to_string()),
            )
            .expect("record");
        }
        let raw = std::fs::read_to_string(&path).expect("read");
        assert!(!raw.contains("secret transcript"));
        assert!(raw.contains("[17 chars]"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn image_paste_round_trips_as_base64() {
        let path = tmpfile("imgpaste.jsonl");
        let _ = std::fs::remove_file(&path);
        let bytes = vec![0u8, 1, 2, 250, 255, 128];
        {
            let mut r = Recorder::open(&path).expect("open");
            r.record_header(&test_header(fixed_ts())).expect("header");
            r.record_msg(
                fixed_ts(),
                &Msg::ClipboardRead(ClipboardRead::Image {
                    bytes: bytes.clone(),
                    format: "png".to_string(),
                }),
            )
            .expect("record");
        }
        let (_, mut replay) = Replay::open(&path).expect("replay");
        let RecordLine::Entry(entry) = replay.next().unwrap().unwrap() else {
            panic!("expected entry");
        };
        match entry.to_msg().unwrap() {
            Msg::ClipboardRead(ClipboardRead::Image {
                bytes: back,
                format,
            }) => {
                assert_eq!(back, bytes, "image bytes must replay bit-exactly");
                assert_eq!(format, "png");
            },
            other => panic!("expected image paste, got {other:?}"),
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn replay_refuses_headerless_recording() {
        let path = tmpfile("headerless.jsonl");
        std::fs::write(
            &path,
            "{\"ts\":\"2026-07-02T12:00:00Z\",\"kind\":\"Tick\",\"turn\":null,\"msg\":\"Tick\"}\n",
        )
        .expect("write");
        let err = Replay::open(&path).expect_err("must refuse");
        assert!(err.to_string().contains("session header"), "got: {err:#}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn replay_classifies_appended_second_session_header() {
        // `Recorder::open` appends, so reusing a --record path produces
        // back-to-back sessions. The reader must surface the second header
        // as a typed line, not a parse error.
        let path = tmpfile("twosessions.jsonl");
        let _ = std::fs::remove_file(&path);
        {
            let mut r = Recorder::open(&path).expect("open");
            r.record_header(&test_header(fixed_ts())).expect("header");
            r.record_msg(fixed_ts(), &Msg::SessionSaved)
                .expect("record");
        }
        {
            let mut r = Recorder::open(&path).expect("reopen");
            r.record_header(&test_header(fixed_ts())).expect("header2");
            r.record_msg(fixed_ts(), &Msg::Quit).expect("record");
        }
        let (_, replay) = Replay::open(&path).expect("replay");
        let lines: Vec<_> = replay.collect::<std::io::Result<_>>().expect("read");
        assert_eq!(lines.len(), 3);
        assert!(matches!(lines[0], RecordLine::Entry(_)));
        assert!(matches!(lines[1], RecordLine::Header(_)));
        assert!(matches!(lines[2], RecordLine::Entry(_)));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn ticks_are_elided_from_recordings() {
        // The reducer's Tick arm is a documented no-op (pinned by the
        // `tick_is_a_reducer_noop` invariant test), so a 60 Hz tick stream
        // adds nothing but bulk — record_msg drops it.
        let path = tmpfile("noticks.jsonl");
        let _ = std::fs::remove_file(&path);
        {
            let mut r = Recorder::open(&path).expect("open");
            r.record_header(&test_header(fixed_ts())).expect("header");
            r.record_msg(fixed_ts(), &Msg::Tick).expect("tick");
            r.record_msg(fixed_ts(), &Msg::Quit).expect("quit");
            r.record_msg(fixed_ts(), &Msg::Tick).expect("tick");
        }
        let (_, replay) = Replay::open(&path).expect("replay");
        let lines: Vec<_> = replay.collect::<std::io::Result<_>>().expect("read");
        assert_eq!(lines.len(), 1, "only the Quit entry may hit disk");
        let RecordLine::Entry(entry) = &lines[0] else {
            panic!("expected entry");
        };
        assert_eq!(entry.kind, "Quit");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn trailer_round_trips_and_fingerprint_is_stable() {
        let path = tmpfile("trailer.jsonl");
        let _ = std::fs::remove_file(&path);
        let session = crate::domain::State::new(
            Config::default(),
            PathBuf::from("/tmp/project"),
            "ollama/test".to_string(),
            fixed_ts(),
        )
        .session;
        {
            let mut r = Recorder::open(&path).expect("open");
            r.record_header(&test_header(fixed_ts())).expect("header");
            r.record_trailer(fixed_ts(), &session).expect("trailer");
        }
        let (_, mut replay) = Replay::open(&path).expect("replay");
        let line = replay.next().expect("line").expect("io ok");
        let RecordLine::Trailer(trailer) = line else {
            panic!("expected trailer, got {line:?}");
        };
        // Same session, same fingerprint — the stability the live-match
        // verdict rests on.
        assert_eq!(
            trailer.final_session_fingerprint,
            session_fingerprint(&session)
        );
        assert!(trailer.final_session_fingerprint.starts_with("sha256:"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn replay_classifies_malformed_line() {
        let path = tmpfile("bad.jsonl");
        let header = serde_json::to_string(&test_header(fixed_ts())).unwrap();
        std::fs::write(&path, format!("{header}\nnot-json\n")).expect("write");
        let (_, mut replay) = Replay::open(&path).expect("open");
        let line = replay.next().expect("line").expect("io ok");
        assert!(matches!(line, RecordLine::Malformed { .. }));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "predates the lint; see .github/baselines/expect_budget.txt"
    )]
    fn every_msg_kind_has_a_round_trip_sample() {
        // Parity guard: each `MsgKind` gets at least one representative
        // sample that must survive serialize → deserialize exactly. The
        // `covered` match is exhaustive over `MsgKind`, so adding a Msg
        // variant without extending the samples is a compile error here.
        use crate::domain::{
            ApprovalKind, ContextUsageSnapshot, Key, KeyCode, KeyMods, PromptTokenBreakdown,
            RuntimeSignal, SlashCmd, StatusKind, ToolCallId, ToolOutcome,
        };
        use mermaid_model::models::ReasoningChunk;

        fn covered(kind: MsgKind) -> bool {
            match kind {
                MsgKind::Key
                | MsgKind::Paste
                | MsgKind::ClipboardRead
                | MsgKind::SubmitPrompt
                | MsgKind::Slash
                | MsgKind::CancelTurn
                | MsgKind::Confirm
                | MsgKind::Quit
                | MsgKind::RuntimeSignal
                | MsgKind::StreamText
                | MsgKind::StreamReasoning
                | MsgKind::StreamToolCall
                | MsgKind::ContextUsageEstimated
                | MsgKind::ProviderContextResolved
                | MsgKind::OllamaPlacementResolved
                | MsgKind::ProviderVisionResolved
                | MsgKind::BuiltinToolSchemaTokens
                | MsgKind::CompactionFinished
                | MsgKind::CompactionFailed
                | MsgKind::StreamDone
                | MsgKind::UpstreamError
                | MsgKind::ToolStarted
                | MsgKind::ToolProgress
                | MsgKind::ToolFinished
                | MsgKind::ApprovalRequested
                | MsgKind::QuestionAsked
                | MsgKind::TasksUpdated
                | MsgKind::TaskNotice
                | MsgKind::TurnCancelled
                | MsgKind::Mcp
                | MsgKind::HookContext
                | MsgKind::InstructionsChanged
                | MsgKind::MemoryChanged
                | MsgKind::SessionProvenanceResolved
                | MsgKind::SessionSaved
                | MsgKind::ConversationLoaded
                | MsgKind::ConversationsListed
                | MsgKind::ProjectFilesListed
                | MsgKind::ScratchpadReady
                | MsgKind::RuntimeStore
                | MsgKind::ModelPullFinished
                | MsgKind::ModelPullProgress
                | MsgKind::Tick
                | MsgKind::Resize
                | MsgKind::MouseScroll
                | MsgKind::FocusChanged
                | MsgKind::OpenImageAt
                | MsgKind::AvailableModelsListed
                | MsgKind::TransientStatus
                | MsgKind::Toast
                | MsgKind::EditorReturned
                | MsgKind::BackgroundAgent
                | MsgKind::CopySelection => true,
            }
        }

        let samples: Vec<Msg> = vec![
            Msg::TasksUpdated {
                store: {
                    let mut store = crate::domain::ChecklistStore::default();
                    store.create(
                        vec![crate::domain::ChecklistSpec {
                            subject: "sample".to_string(),
                            active_form: "sampling".to_string(),
                            description: None,
                            in_progress: true,
                        }],
                        crate::domain::ChecklistOrigin::Model,
                        crate::domain::Stamp {
                            now_epoch: 10,
                            run_tokens: 20,
                        },
                    );
                    store
                },
            },
            Msg::TaskNotice {
                text: "The user edited the task checklist: Added task #1 'x'.".to_string(),
            },
            Msg::Key(Key {
                code: KeyCode::Char('x'),
                modifiers: KeyMods::ctrl(),
            }),
            Msg::Key(Key {
                code: KeyCode::PageUp,
                modifiers: KeyMods::NONE,
            }),
            Msg::Paste(Paste::Text("pasted".to_string())),
            Msg::ClipboardRead(ClipboardRead::Image {
                bytes: vec![9, 8, 7],
                format: "png".to_string(),
            }),
            Msg::SubmitPrompt {
                text: "prompt".to_string(),
                attachment_ids: vec![1],
            },
            Msg::Slash(SlashCmd::Model(Some("anthropic/opus".to_string()))),
            Msg::HookContext {
                turn: TurnId(2),
                texts: vec!["hook says hi".to_string()],
            },
            Msg::Slash(SlashCmd::Compact(None)),
            Msg::CancelTurn,
            Msg::BackgroundAgentStarted {
                agent_id: "a7".to_string(),
                description: "audit docs".to_string(),
            },
            Msg::BackgroundAgentProgress {
                agent_id: "a7".to_string(),
                activity: "read_file…".to_string(),
                tokens: 1200,
            },
            Msg::BackgroundAgentFinished {
                agent_id: "a7".to_string(),
                description: "audit docs".to_string(),
                report: "all good".to_string(),
                success: true,
                cancelled: false,
                usage: Some(mermaid_model::models::TokenUsage::provider(60_000, 30_000)),
                tokens: 90_000,
                duration_secs: 132,
            },
            Msg::ConfirmAccepted,
            Msg::ConfirmDeclined,
            Msg::Quit,
            Msg::RuntimeSignal(RuntimeSignal::Terminate),
            Msg::StreamText {
                turn: TurnId(1),
                chunk: "chunk".to_string(),
            },
            Msg::StreamReasoning {
                turn: TurnId(1),
                chunk: ReasoningChunk {
                    text: "thinking".to_string(),
                    signature: Some("sig".to_string()),
                },
            },
            Msg::StreamToolCall {
                turn: TurnId(1),
                call: mermaid_model::models::tool_call::ToolCall {
                    id: Some("call_1".to_string()),
                    function: mermaid_model::models::tool_call::FunctionCall {
                        name: "read_file".to_string(),
                        arguments: serde_json::json!({"path": "src/main.rs"}),
                    },
                },
            },
            Msg::ContextUsageEstimated {
                turn: TurnId(1),
                snapshot: ContextUsageSnapshot::from_estimate(
                    PromptTokenBreakdown {
                        system_tokens: 10,
                        instructions_tokens: 5,
                        message_tokens: 20,
                        tool_schema_tokens: 30,
                        image_count: 0,
                        message_count: 2,
                        tool_count: 3,
                    },
                    Some(128_000),
                ),
            },
            Msg::ProviderContextResolved {
                model_id: "m".to_string(),
                model_max: Some(131_072),
                effective: Some(32_768),
                source: None,
                max_output: Some(64_000),
            },
            Msg::OllamaPlacementResolved {
                model_id: "m".to_string(),
                size_vram_bytes: 1,
                total_bytes: 2,
                suggested_num_ctx: Some(8192),
            },
            Msg::ProviderVisionResolved {
                model_id: "m".to_string(),
                supports_vision: Some(false),
                warn: true,
            },
            Msg::BuiltinToolSchemaTokens(1234),
            Msg::CompactionFailed {
                turn: TurnId(2),
                trigger: crate::domain::CompactionTrigger::Manual,
                message: "nothing to do".to_string(),
                kind: StatusKind::Info,
            },
            Msg::CompactionFinished {
                turn: TurnId(2),
                result: crate::domain::CompactionResult {
                    record: crate::domain::CompactionEvent {
                        id: "c1".to_string(),
                        trigger: crate::domain::CompactionTrigger::Manual,
                        created_at: fixed_ts(),
                        before_tokens: 1000,
                        after_tokens: 100,
                        archived_message_count: 8,
                        preserved_message_count: 2,
                        preserved_turn_count: 1,
                        summary_tokens: 90,
                        duration_secs: 1.5,
                        review_status: crate::domain::CompactionReviewStatus::Reviewed,
                        review_error: None,
                        focus: None,
                        archive_path: None,
                    },
                    replacement_messages: vec![mermaid_model::models::ChatMessage::system(
                        "checkpoint",
                    )],
                    archived_messages: vec![mermaid_model::models::ChatMessage::user("old")],
                    before_snapshot: ContextUsageSnapshot::from_estimate(
                        PromptTokenBreakdown::default(),
                        Some(128_000),
                    ),
                    after_snapshot: ContextUsageSnapshot::from_estimate(
                        PromptTokenBreakdown::default(),
                        Some(128_000),
                    ),
                    usage: None,
                    source_boundaries: Vec::new(),
                },
            },
            Msg::UpstreamError {
                turn: TurnId(1),
                error: mermaid_model::models::UserFacingError {
                    summary: "Rate limited".to_string(),
                    message: "429 too many requests".to_string(),
                    suggestion: "retry in a moment".to_string(),
                    category: mermaid_model::models::ErrorCategory::Temporary,
                    recoverable: true,
                },
            },
            Msg::StreamDone {
                turn: TurnId(1),
                usage: Some(mermaid_model::models::TokenUsage::provider(10, 5)),
                provider_continuation: None,
                stop_reason: Some(mermaid_model::models::FinishReason::Stop),
            },
            Msg::TurnCancelled(TurnId(3)),
            Msg::ToolStarted {
                turn: TurnId(1),
                call_id: ToolCallId(1),
            },
            Msg::ToolProgress {
                turn: TurnId(1),
                call_id: ToolCallId(1),
                event: crate::domain::ProgressEvent::Artifact {
                    mime: "image/png".to_string(),
                    data: vec![1, 2, 3],
                    caption: Some("shot".to_string()),
                },
            },
            Msg::ToolFinished {
                turn: TurnId(1),
                call_id: ToolCallId(1),
                outcome: ToolOutcome::success("out", "read 3 lines", 0.5),
            },
            Msg::ApprovalRequested {
                turn: TurnId(1),
                call_id: ToolCallId(2),
                tool: "execute_command".to_string(),
                risk: "destructive".to_string(),
                kind: ApprovalKind::Shell,
                prompt: "rm -rf build".to_string(),
                allowlist_scope: "exact".to_string(),
            },
            Msg::McpServerReady {
                name: "srv".to_string(),
                tools: vec![crate::domain::McpToolSpec {
                    name: "mcp__srv__t".to_string(),
                    raw_name: "t".to_string(),
                    description: "d".to_string(),
                    input_schema: serde_json::json!({"type": "object"}),
                    read_only_hint: false,
                }],
            },
            Msg::McpServerErrored {
                name: "srv".to_string(),
                reason: "exit 1".to_string(),
            },
            Msg::McpServerStopped {
                name: "srv".to_string(),
            },
            Msg::InstructionsChanged(None),
            Msg::MemoryChanged(None),
            Msg::SessionProvenanceResolved(crate::domain::SessionProvenance {
                git_branch: Some("main".to_string()),
                git_sha: Some("a614aa9f".to_string()),
                cli_version: Some("0.21.1".to_string()),
            }),
            Msg::SessionSaved,
            Msg::ConversationLoaded(ConversationHistory::new(
                "/p".to_string(),
                "m".to_string(),
                fixed_ts(),
            )),
            Msg::ConversationsListed(vec![crate::domain::ConversationSummary {
                id: "20260702_120000_123".to_string(),
                title: "t".to_string(),
                message_count: 1,
                updated_at: "2026-07-02".to_string(),
            }]),
            Msg::ProjectFilesListed(vec!["src/main.rs".to_string(), "docs/".to_string()]),
            Msg::ScratchpadReady {
                session_id: "20260702_120000_123".to_string(),
                path: std::path::PathBuf::from("/data/tmp/scratchpad/-proj/20260702_120000_123"),
            },
            Msg::RuntimeText("daemon says hi".to_string()),
            Msg::RuntimeTasksListed(Vec::new()),
            Msg::RuntimeTaskLoaded {
                task: None,
                events: Vec::new(),
            },
            Msg::RuntimeProcessesListed(Vec::new()),
            Msg::RuntimeApprovalsListed(Vec::new()),
            Msg::RuntimeCheckpointsListed(Vec::new()),
            Msg::ForkCheckpointsFound(Vec::new()),
            Msg::RuntimePluginsListed(Vec::new()),
            Msg::ModelPullFinished {
                model: "qwen3".to_string(),
            },
            Msg::ModelPullProgress("pulling 42%".to_string()),
            Msg::Tick,
            Msg::Resize {
                width: 120,
                height: 40,
            },
            Msg::TransientStatus {
                text: "saved".to_string(),
            },
            Msg::MouseScroll { delta: -3 },
            Msg::FocusChanged(false),
            Msg::OpenImageAt {
                message_index: 4,
                image_index: 0,
                image_number: None,
            },
            Msg::EditorReturned {
                text: Some("edited draft".to_string()),
            },
            Msg::CopySelection("copied".to_string()),
        ];

        // Every MsgKind must appear in the sample set. `covered` is the
        // compile-time guard (exhaustive match breaks when Msg grows); this
        // list is the runtime completeness check against the samples.
        let seen: Vec<MsgKind> = samples.iter().map(|m| m.kind()).collect();
        let missing: Vec<String> = [
            MsgKind::Key,
            MsgKind::Paste,
            MsgKind::ClipboardRead,
            MsgKind::SubmitPrompt,
            MsgKind::Slash,
            MsgKind::CancelTurn,
            MsgKind::Confirm,
            MsgKind::Quit,
            MsgKind::RuntimeSignal,
            MsgKind::StreamText,
            MsgKind::StreamReasoning,
            MsgKind::StreamToolCall,
            MsgKind::ContextUsageEstimated,
            MsgKind::ProviderContextResolved,
            MsgKind::OllamaPlacementResolved,
            MsgKind::ProviderVisionResolved,
            MsgKind::BuiltinToolSchemaTokens,
            MsgKind::CompactionFinished,
            MsgKind::CompactionFailed,
            MsgKind::StreamDone,
            MsgKind::UpstreamError,
            MsgKind::ToolStarted,
            MsgKind::ToolProgress,
            MsgKind::ToolFinished,
            MsgKind::ApprovalRequested,
            MsgKind::TurnCancelled,
            MsgKind::Mcp,
            MsgKind::HookContext,
            MsgKind::InstructionsChanged,
            MsgKind::MemoryChanged,
            MsgKind::SessionProvenanceResolved,
            MsgKind::SessionSaved,
            MsgKind::ConversationLoaded,
            MsgKind::ConversationsListed,
            MsgKind::ProjectFilesListed,
            MsgKind::RuntimeStore,
            MsgKind::ModelPullFinished,
            MsgKind::ModelPullProgress,
            MsgKind::Tick,
            MsgKind::Resize,
            MsgKind::MouseScroll,
            MsgKind::FocusChanged,
            MsgKind::OpenImageAt,
            MsgKind::TransientStatus,
            MsgKind::CopySelection,
        ]
        .iter()
        .filter(|k| covered(**k) && !seen.contains(k))
        .map(|k| format!("{k:?}"))
        .collect();
        assert!(
            missing.is_empty(),
            "MsgKinds without a round-trip sample: {missing:?}"
        );

        // …and every sample must survive the round trip bit-exactly.
        for msg in &samples {
            let value = serde_json::to_value(msg).expect("serialize");
            let back: Msg = serde_json::from_value(value.clone())
                .unwrap_or_else(|e| panic!("deserialize {value}: {e}"));
            assert_eq!(
                format!("{msg:?}"),
                format!("{back:?}"),
                "round trip changed the msg"
            );
        }
    }
}
