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
//!   - `body`: the `Msg` itself, serde-serialized.
//!
//! Not every `Msg` field is safely serializable today — raw image
//! bytes in `Paste::Image`, for example. Unsupported variants are
//! skipped on record (the reducer still sees them; they just don't
//! land in the log). Replay is a best-effort reconstruction.
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

use crate::domain::{MsgKind, TurnId};

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
}
