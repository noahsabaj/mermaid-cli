//! `--replay <file>`: fold a recorded `Msg` log back through the pure
//! reducer and reconstruct the session it describes.
//!
//! This is the payoff of the MVU architecture's purity contract: because
//! `update(State, Msg)` reads its clock from `state.now` (injected data)
//! and mints ids from an in-state counter, replay is a straight fold —
//! rebuild the initial `State` from the recording's [`SessionHeader`],
//! stamp each entry's recorded `ts` into `state.now`, apply the entry's
//! `Msg`, and drop the emitted `Cmd`s (their real-world results are already
//! in the log as later `Msg`s).
//!
//! Replay is headless and effect-free: no model calls, no tool execution,
//! no terminal UI, no reads of the live machine's config (the header embeds
//! a snapshot). It exists for post-mortem debugging — "what state was the
//! reducer in when this went wrong?" — and as a standing determinism check:
//! every replay folds the log twice and verifies both folds produce
//! identical state, failing loudly (exit 1) if the reducer has grown a
//! nondeterminism bug.

use std::path::Path;

use anyhow::Result;
use chrono::{DateTime, Local};

use crate::app::recorder::{RecordLine, Replay, SessionHeader, session_fingerprint};
use mermaid_domain::{Msg, State, update};
use mermaid_model::models::MessageRole;

/// Everything `--replay` learned from one recording.
pub struct ReplayReport {
    pub header: SessionHeader,
    /// Entry lines seen for this session (excludes the header line).
    pub total_entries: usize,
    /// Entries successfully reconstructed and folded through `update`.
    pub applied: usize,
    /// `(file line number, reason)` for every line that could not be
    /// reconstructed — malformed JSON, or a `Msg` variant this build doesn't
    /// know (log written by a newer mermaid).
    pub skipped: Vec<(usize, String)>,
    /// File line number where the fold stopped because the recorded session
    /// quit (`state.should_exit`), mirroring the live loop's exit.
    pub stopped_at: Option<usize>,
    /// Entries after the stop line that were parsed but not applied.
    pub not_applied_after_stop: usize,
    /// Line number of a second [`SessionHeader`] if the file holds multiple
    /// appended sessions. Only the first session is replayed.
    pub second_session_at: Option<usize>,
    /// True when folding the log twice produced byte-identical `State`
    /// debug representations — the reducer-purity invariant.
    pub deterministic: bool,
    /// The live session's final fingerprint, when the recording was sealed
    /// on clean exit. `None` for crashed sessions and pre-trailer logs.
    pub live_fingerprint: Option<String>,
    /// Whether the folded session reproduces the live one: `Some(true)` =
    /// exact match, `Some(false)` = diverged (expected when redaction fired
    /// mid-session or the reducer changed since recording), `None` = no
    /// trailer to compare against.
    pub matches_live: Option<bool>,
    /// Final state after the fold.
    pub state: State,
}

/// Read `path`, fold its first session through the reducer (twice, for the
/// determinism verdict), and report.
pub fn replay_recording(path: &Path) -> Result<ReplayReport> {
    let (header, lines) = Replay::open(path)?;

    // Reconstruct everything up front so the fold can run twice without
    // re-reading the file.
    let mut msgs: Vec<(usize, DateTime<Local>, Msg)> = Vec::new();
    let mut skipped = Vec::new();
    let mut second_session_at = None;
    let mut trailer = None;
    let mut line_no = 1usize; // the header is line 1
    for line in lines {
        line_no += 1;
        match line? {
            RecordLine::Entry(entry) => match entry.to_msg() {
                Ok(msg) => msgs.push((line_no, entry.ts, msg)),
                Err(err) => skipped.push((line_no, format!("{err:#}"))),
            },
            RecordLine::Trailer(t) => {
                // Clean-exit seal: the live session's final fingerprint.
                // Keep the first (this session's); keep reading in case an
                // appended second session follows.
                trailer.get_or_insert(t);
            },
            RecordLine::Header(_) => {
                // `Recorder::open` appends, so a reused --record path holds
                // several sessions back to back. Replay the first; stop here.
                second_session_at = Some(line_no);
                break;
            },
            RecordLine::Malformed { error, .. } => {
                skipped.push((line_no, format!("malformed line: {error}")));
            },
        }
    }
    let total_entries = msgs.len() + skipped.len();

    let (state, stopped_at) = fold(&header, &msgs);
    let (second, _) = fold(&header, &msgs);
    let deterministic = format!("{state:?}") == format!("{second:?}");

    // Verify the fold against the LIVE session's recorded outcome (a
    // stronger claim than determinism, which only proves self-consistency).
    let live_fingerprint = trailer.map(|t| t.final_session_fingerprint);
    let matches_live = live_fingerprint
        .as_ref()
        .map(|live| *live == session_fingerprint(&state.session));

    let not_applied_after_stop = match stopped_at {
        Some(stop) => msgs.iter().filter(|(line, ..)| *line > stop).count(),
        None => 0,
    };
    let applied = msgs.len() - not_applied_after_stop;

    Ok(ReplayReport {
        header,
        total_entries,
        applied,
        skipped,
        stopped_at,
        not_applied_after_stop,
        second_session_at,
        deterministic,
        live_fingerprint,
        matches_live,
        state,
    })
}

/// The fold itself: initial `State` from the header, then one `update` per
/// entry under the entry's recorded clock. Emitted `Cmd`s are dropped — the
/// log already contains every effect result as a later `Msg`. Stops where
/// the live loop would have: on `should_exit`.
fn fold(header: &SessionHeader, msgs: &[(usize, DateTime<Local>, Msg)]) -> (State, Option<usize>) {
    let mut state = State::new(
        header.config.clone(),
        header.cwd.clone(),
        header.model_id.clone(),
        header.ts,
    );
    if let Some(seed) = header.seed_conversation.clone() {
        state.seed_conversation(seed);
    }
    let mut stopped_at = None;
    for (line, ts, msg) in msgs {
        state.now = *ts;
        let (next, _cmds) = update(state, msg.clone());
        state = next;
        if state.should_exit {
            stopped_at = Some(*line);
            break;
        }
    }
    (state, stopped_at)
}

/// CLI entry point for `--replay`: fold, print the report + reconstructed
/// transcript to stdout, and return the determinism verdict (`false` means
/// the caller should exit non-zero — the purity invariant is broken).
pub fn run_replay(path: &Path) -> Result<bool> {
    let report = replay_recording(path)?;
    print!("{}", render_report(path, &report));
    Ok(report.deterministic)
}

/// Plain-text report. Separate from `run_replay` so tests can assert on it.
#[expect(
    clippy::too_many_lines,
    reason = "predates the lint; see .github/baselines/expect_budget.txt"
)]
fn render_report(path: &Path, report: &ReplayReport) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    let h = &report.header;
    let _ = writeln!(out, "replay: {}", path.display());
    let _ = writeln!(
        out,
        "  recorded: {} · model: {} · cwd: {}",
        h.ts.to_rfc3339(),
        h.model_id,
        h.cwd.display()
    );
    match &h.seed_conversation {
        Some(seed) => {
            let _ = writeln!(
                out,
                "  seed: continued from {} ({} messages)",
                seed.id,
                seed.messages().len()
            );
        },
        None => {
            let _ = writeln!(out, "  seed: fresh session");
        },
    }
    let _ = writeln!(
        out,
        "  entries: {} total · {} applied · {} skipped",
        report.total_entries,
        report.applied,
        report.skipped.len()
    );
    for (line, reason) in &report.skipped {
        let _ = writeln!(out, "    line {line}: {reason}");
    }
    if let Some(stop) = report.stopped_at {
        let _ = writeln!(
            out,
            "  note: session quit at line {stop}; {} later entries not applied",
            report.not_applied_after_stop
        );
    }
    if let Some(line) = report.second_session_at {
        let _ = writeln!(
            out,
            "  note: a second appended session starts at line {line} — replayed the first only"
        );
    }
    let _ = writeln!(
        out,
        "  determinism: {}",
        if report.deterministic {
            "PASS — folding the log twice produced identical state"
        } else {
            "FAIL — two folds of the same log diverged (reducer purity bug)"
        }
    );
    let _ = writeln!(
        out,
        "  live match: {}",
        match report.matches_live {
            Some(true) => "yes — the fold reproduces the live session's recorded outcome",
            Some(false) =>
                "no — the fold differs from the live session (expected if redaction \
                 fired mid-session or the reducer changed since recording)",
            None => "unknown — recording has no clean-exit fingerprint",
        }
    );

    let messages = report.state.session.messages();
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "transcript: {} — {} messages",
        report.state.session.conversation.title,
        messages.len()
    );
    for msg in messages {
        let role = match msg.role {
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::System => "system",
            MessageRole::Tool => "tool",
        };
        // Indent continuation lines so multi-line content stays visually
        // attached to its role tag.
        let mut lines = msg.content.lines();
        let first = lines.next().unwrap_or_default();
        let _ = writeln!(out, "  [{role}] {first}");
        for line in lines {
            let _ = writeln!(out, "    {line}");
        }
        if let Some(calls) = &msg.tool_calls
            && !calls.is_empty()
        {
            let names: Vec<&str> = calls.iter().map(|c| c.function.name.as_str()).collect();
            let _ = writeln!(
                out,
                "    ({} tool calls: {})",
                calls.len(),
                names.join(", ")
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::recorder::{RECORDING_FORMAT_VERSION, Recorder};
    use mermaid_domain::Config;
    use mermaid_domain::TurnId;
    use mermaid_model::models::{FinishReason, TokenUsage};
    use std::path::PathBuf;

    fn tmpfile(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("mermaid_replay_tests");
        let _ = std::fs::create_dir_all(&dir);
        dir.join(name)
    }

    fn fixed_ts(offset_secs: i64) -> DateTime<Local> {
        chrono::DateTime::parse_from_rfc3339("2026-07-02T12:00:00.500+00:00")
            .unwrap()
            .with_timezone(&Local)
            + chrono::Duration::seconds(offset_secs)
    }

    fn header(ts: DateTime<Local>) -> SessionHeader {
        SessionHeader {
            format: RECORDING_FORMAT_VERSION,
            ts,
            model_id: "ollama/test".to_string(),
            cwd: PathBuf::from("/tmp/replay-project"),
            config: Config::default(),
            seed_conversation: None,
        }
    }

    /// Record a small but realistic session — prompt, streamed answer,
    /// stream end, quit — exactly as the live driver would.
    fn record_session(path: &PathBuf) {
        let _ = std::fs::remove_file(path);
        let h = header(fixed_ts(0));
        let mut r = Recorder::open(path).expect("open recorder");
        r.record_header(&h).expect("header");
        // Fold while recording, exactly like the live driver: one shared
        // clock per entry, and the trailer seals the final session.
        let mut state = State::new(h.config.clone(), h.cwd.clone(), h.model_id.clone(), h.ts);
        let script: Vec<(i64, Msg)> = vec![
            (
                1,
                Msg::SubmitPrompt {
                    text: "hello there".to_string(),
                    attachment_ids: Vec::new(),
                },
            ),
            (
                2,
                Msg::StreamText {
                    turn: TurnId(1),
                    chunk: "Hi — ".to_string(),
                },
            ),
            (
                3,
                Msg::StreamText {
                    turn: TurnId(1),
                    chunk: "hello!".to_string(),
                },
            ),
            (
                4,
                Msg::StreamDone {
                    turn: TurnId(1),
                    usage: Some(TokenUsage::provider(10, 5)),
                    provider_continuation: None,
                    stop_reason: Some(FinishReason::Stop),
                },
            ),
            (5, Msg::Quit),
        ];
        for (offset, msg) in script {
            let now = fixed_ts(offset);
            r.record_msg(now, &msg).expect("record");
            state.now = now;
            let (next, _cmds) = update(state, msg);
            state = next;
        }
        r.record_trailer(fixed_ts(9), &state.session)
            .expect("trailer");
        r.flush().expect("flush");
    }

    #[test]
    fn replay_reconstructs_a_recorded_session_deterministically() {
        let path = tmpfile("session.jsonl");
        record_session(&path);

        let report = replay_recording(&path).expect("replay");
        assert!(
            report.deterministic,
            "double fold must produce identical state"
        );
        assert_eq!(report.total_entries, 5);
        assert_eq!(report.applied, 5, "skipped: {:?}", report.skipped);
        assert!(report.skipped.is_empty());
        assert_eq!(report.stopped_at, Some(6), "Quit is line 6 of the file");
        assert!(report.state.should_exit);

        // The reconstructed transcript holds the prompt and the assembled
        // streamed answer.
        let messages = report.state.session.messages();
        let user = messages
            .iter()
            .find(|m| m.role == MessageRole::User)
            .expect("user message");
        assert_eq!(user.content, "hello there");
        let assistant = messages
            .iter()
            .find(|m| m.role == MessageRole::Assistant)
            .expect("assistant message");
        assert_eq!(assistant.content, "Hi — hello!");

        // Clock injection end to end: the initial conversation id derives
        // from the recorded header ts, not from the machine's wall clock.
        let expected_id = format!("{}", fixed_ts(0).format("%Y%m%d_%H%M%S_%3f"));
        assert_eq!(report.state.session.conversation.id, expected_id);

        // Message commit timestamps come from the recorded per-entry clock.
        assert_eq!(user.timestamp, fixed_ts(1));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn replay_report_renders_transcript_and_verdict() {
        let path = tmpfile("render.jsonl");
        record_session(&path);
        let report = replay_recording(&path).expect("replay");
        let text = render_report(&path, &report);
        assert!(text.contains("determinism: PASS"));
        assert!(text.contains("live match: yes"));
        assert!(text.contains("[user] hello there"));
        assert!(text.contains("[assistant] Hi — hello!"));
        assert!(text.contains("5 applied"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn replay_verifies_against_the_live_fingerprint() {
        let path = tmpfile("livematch.jsonl");
        record_session(&path);
        let report = replay_recording(&path).expect("replay");
        assert_eq!(
            report.matches_live,
            Some(true),
            "a faithful fold must match the live session's seal"
        );
        assert!(
            report
                .live_fingerprint
                .as_deref()
                .expect("trailer present")
                .starts_with("sha256:")
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn tampered_recording_fails_live_match_but_stays_deterministic() {
        let path = tmpfile("tamper.jsonl");
        record_session(&path);
        // Alter one streamed chunk after the fact — the fold is still
        // self-consistent (deterministic) but no longer reproduces what the
        // live session saw.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("hello!"));
        std::fs::write(&path, raw.replace("hello!", "goodbye")).unwrap();

        let report = replay_recording(&path).expect("replay");
        assert_eq!(
            report.matches_live,
            Some(false),
            "an altered log must not match the live fingerprint"
        );
        assert!(
            report.deterministic,
            "tampering must not affect fold self-consistency"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn recording_without_trailer_reports_unknown_live_match() {
        // A crashed session never writes the clean-exit seal.
        let path = tmpfile("crash.jsonl");
        record_session(&path);
        let raw = std::fs::read_to_string(&path).unwrap();
        let without_trailer: String = raw
            .lines()
            .filter(|l| !l.contains("final_session_fingerprint"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(&path, without_trailer).unwrap();

        let report = replay_recording(&path).expect("replay");
        assert_eq!(report.matches_live, None);
        assert!(report.live_fingerprint.is_none());
        assert!(render_report(&path, &report).contains("live match: unknown"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn replay_stops_at_second_appended_session() {
        let path = tmpfile("two.jsonl");
        record_session(&path);
        // Append a second session the way a reused --record path would.
        {
            let mut r = Recorder::open(&path).expect("reopen");
            r.record_header(&header(fixed_ts(100))).expect("header2");
            r.record_msg(fixed_ts(101), &Msg::SessionSaved)
                .expect("record");
        }
        let report = replay_recording(&path).expect("replay");
        // File: header(1) + 5 entries(2-6) + trailer(7) + header2(8).
        assert_eq!(report.second_session_at, Some(8));
        assert_eq!(report.total_entries, 5, "second session must not count");
        assert_eq!(
            report.matches_live,
            Some(true),
            "the first session's trailer still verifies"
        );
        assert!(report.deterministic);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn replay_skips_unknown_msg_variants_with_line_numbers() {
        let path = tmpfile("unknown.jsonl");
        record_session(&path);
        // Simulate a log from a newer mermaid: an entry whose msg variant
        // this build doesn't know. Insert before the Quit line.
        let raw = std::fs::read_to_string(&path).unwrap();
        let mut lines: Vec<&str> = raw.lines().collect();
        let future = r#"{"ts":"2026-07-02T12:00:04.700+00:00","kind":"HoloDeck","turn":null,"msg":{"HoloDeck":{"program":"bridge"}}}"#;
        lines.insert(5, future);
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();

        let report = replay_recording(&path).expect("replay");
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].0, 6, "inserted at file line 6");
        assert!(report.skipped[0].1.contains("HoloDeck"));
        assert!(report.deterministic);
        // Everything else still applied.
        assert_eq!(report.applied, 5);
        let _ = std::fs::remove_file(&path);
    }

    /// Manual QA helper: writes a small recording to
    /// `$MERMAID_REPLAY_FIXTURE` (default `/tmp/mermaid-replay-fixture.jsonl`)
    /// so the real CLI can be exercised end to end:
    /// `cargo test --lib write_replay_fixture -- --ignored && mermaid --replay <path>`
    #[test]
    #[ignore = "writes a fixture for manual --replay QA"]
    fn write_replay_fixture() {
        let path = std::env::var("MERMAID_REPLAY_FIXTURE")
            .unwrap_or_else(|_| "/tmp/mermaid-replay-fixture.jsonl".to_string());
        let path = PathBuf::from(path);
        record_session(&path);
        eprintln!("fixture written: {}", path.display());
    }

    #[test]
    fn same_log_folded_under_different_wall_clock_is_identical() {
        // The whole point of clock injection: replaying tomorrow gives the
        // same state as replaying today. Fold the same file twice with real
        // wall time passing between folds and compare.
        let path = tmpfile("stable.jsonl");
        record_session(&path);
        let a = replay_recording(&path).expect("first");
        std::thread::sleep(std::time::Duration::from_millis(25));
        let b = replay_recording(&path).expect("second");
        assert_eq!(
            format!("{:?}", a.state),
            format!("{:?}", b.state),
            "replay must not depend on the machine's wall clock"
        );
        let _ = std::fs::remove_file(&path);
    }
}
