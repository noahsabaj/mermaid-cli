//! Drive the real `mermaid` binary on a pseudo-terminal and read what it drew.
//!
//! Shared by the behavioral pty tests (`pty_visual.rs`) and the golden-frame
//! tests (`pty_frame.rs`).
//!
//! A TUI behaves completely differently when its output is not a terminal: pipe
//! it to a file and it detects "not a tty", skips raw mode, and never draws a
//! frame. A pty makes the program believe it is talking to a real terminal, so
//! it enters raw mode, paints, and responds to keys — while the test holds the
//! other end.
//!
//! Two non-obvious things are required to make that work, both found the hard
//! way:
//!
//!   * **`ESC[6n` must be answered.** crossterm asks the terminal for the
//!     cursor position and blocks on the reply. Nothing answers under a bare
//!     pty, so mermaid hung at startup until the harness replied `ESC[1;1R`.
//!   * **Both pty ends must stay owned.** Dropping the master closes the ConPTY
//!     on Windows, and the next keystroke comes back `BrokenPipe` — which reads
//!     exactly like the app crashing.

#![allow(dead_code)] // Each test binary uses a different slice of this.

pub mod stub_model;

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

/// Ctrl+L. Ratatui emits only the cells that CHANGED, so a mode flip can land
/// as a three-byte patch with no surrounding context. Forcing a full repaint
/// before a read means the buffer holds a whole frame.
pub const CTRL_L: &[u8] = &[0x0c];
/// Shift+Tab as the terminal sends it.
pub const SHIFT_TAB: &[u8] = b"\x1b[Z";
/// Enter and Esc as the terminal sends them.
pub const ENTER: &[u8] = b"\r";
pub const ESC: &[u8] = b"\x1b";

/// Terminal geometry. Fixed, because the snapshots are cell-exact.
pub const ROWS: u16 = 30;
pub const COLS: u16 = 100;

/// A model id no provider resolves. The TUI still comes up, and nothing
/// reaches the network.
const TEST_MODEL: &str = "anthropic/pty-visual-test";

/// How `settled_frame` decides a repaint has finished. Polling for a grid that
/// stopped changing beats sleeping a fixed span twice over: it is ~4x faster
/// when the app repaints promptly, and on a loaded runner it keeps waiting
/// instead of snapshotting a half-applied diff.
const SETTLE_POLL: Duration = Duration::from_millis(40);
/// Consecutive identical reads that count as settled. Three at 40ms is 120ms of
/// provable quiet — ratatui writes a frame in one burst, so a gap that long
/// mid-repaint does not happen.
const SETTLE_STABLE_POLLS: u8 = 3;
/// Hard cap, equal to the flat 500ms + 200ms this replaced: the slow path is no
/// less settled than it used to be.
const SETTLE_MAX: Duration = Duration::from_millis(700);
/// Gap between `wait_for_*` polls. Purely how late a wait notices a condition
/// that is ALREADY true, so shortening it cannot make a wait give up sooner —
/// the deadline is wall-clock — it only stops each satisfied wait from paying
/// up to 100ms of nothing.
const WAIT_POLL: Duration = Duration::from_millis(25);

pub struct Terminal {
    output: Arc<Mutex<Vec<u8>>>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    /// How many `ESC[6n` queries have already been answered.
    answered: usize,
    /// Both ends stay owned for the lifetime of the harness — see the module
    /// docs. The master additionally serves [`Terminal::resize`].
    master: Box<dyn portable_pty::MasterPty + Send>,
    _slave: Box<dyn portable_pty::SlavePty + Send>,
    /// The sandbox root, kept so `normalize` can redact it out of frames.
    sandbox: PathBuf,
    /// Current grid geometry. Starts at [`ROWS`]x[`COLS`]; changes only via
    /// [`Terminal::resize`]. `frame` parses at this size.
    rows: u16,
    cols: u16,
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Terminal {
    pub fn launch(tag: &str) -> Self {
        Self::spawn(tag, &[])
    }

    /// Launch with a transcript already on disk, loaded via `--resume`.
    ///
    /// This is what makes chat rendering snapshot-able at all: the assistant
    /// content is fixed by the test rather than produced by a model, so the
    /// frame is deterministic and no network call happens.
    pub fn launch_with_transcript(tag: &str, prompt: &str, reply: &str) -> Self {
        // Ids are `YYYYMMDD_HHMMSS_mmm` and validated on load; a fixed one
        // keeps the frame stable.
        const ID: &str = "20260101_120000_000";
        let sandbox = test_sandbox(tag);
        let project = sandbox.join("project");
        let conversations = project.join(".mermaid").join("conversations");
        std::fs::create_dir_all(&conversations).expect("create conversations dir");
        write_conversation(&conversations.join(format!("{ID}.json")), ID, prompt, reply);
        Self::spawn_in(tag, sandbox, &["--resume", ID])
    }

    fn spawn(tag: &str, extra_args: &[&str]) -> Self {
        Self::spawn_in(tag, test_sandbox(tag), extra_args)
    }

    fn spawn_in(_tag: &str, sandbox: PathBuf, extra_args: &[&str]) -> Self {
        let binary = env!("CARGO_BIN_EXE_mermaid");
        let home = sandbox.join("home");
        let config = sandbox.join("config");
        let project = sandbox.join("project");
        for dir in [&home, &config, &project] {
            std::fs::create_dir_all(dir).expect("create sandbox dir");
        }

        let pair = native_pty_system()
            .openpty(PtySize {
                rows: ROWS,
                cols: COLS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open pty");

        let output = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&output);
        let mut reader = pair.master.try_clone_reader().expect("clone pty reader");
        std::thread::spawn(move || {
            let mut buf = [0_u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => sink.lock().expect("output lock").extend(&buf[..n]),
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        });

        let mut cmd = CommandBuilder::new(binary);
        cmd.args(["--model", TEST_MODEL]);
        for arg in extra_args {
            cmd.arg(arg);
        }
        cmd.cwd(project.as_os_str());
        cmd.env("HOME", home.as_os_str());
        cmd.env("USERPROFILE", home.as_os_str());
        cmd.env("XDG_CONFIG_HOME", config.as_os_str());
        // The Mermaid-specific overrides are the actual isolation: they work
        // on every platform, where `HOME`/`XDG_CONFIG_HOME` redirect only
        // unix (Windows resolves known folders no environment variable
        // moves). Before them, every Windows run of this harness wrote
        // `last_used_model = "anthropic/pty-visual-test"` into the
        // developer's real `config.toml`. HOME/XDG stay set for whatever
        // else a spawned shell or tool resolves against them.
        cmd.env("MERMAID_CONFIG_DIR", config.join("mermaid").as_os_str());
        cmd.env(
            "MERMAID_DATA_DIR",
            sandbox.join("data").join("mermaid").as_os_str(),
        );
        cmd.env("RUST_BACKTRACE", "0");

        let child = pair.slave.spawn_command(cmd).expect("spawn mermaid in pty");
        let writer = pair.master.take_writer().expect("take pty writer");

        let mut term = Self {
            output,
            writer,
            child,
            answered: 0,
            master: pair.master,
            _slave: pair.slave,
            sandbox,
            rows: ROWS,
            cols: COLS,
        };
        assert!(
            term.wait(
                |text| text.contains("\x1b[?1049h") && text.contains("safety:"),
                Duration::from_secs(30),
            ),
            "mermaid never drew a footer. exited={:?}\n{}",
            term.child.try_wait(),
            term.frame_text(),
        );
        term
    }

    pub fn press(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).expect("write key");
        self.writer.flush().expect("flush key");
    }

    /// Change the terminal geometry mid-session, the way a user drags a
    /// window edge. The app receives a real resize event through the pty.
    ///
    /// The accumulated byte stream was painted for the OLD geometry; parsing
    /// it at the new size would misplace every absolute cursor move. So the
    /// buffer restarts empty, and the next read reflects only what the app
    /// drew for the new grid.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.output.lock().expect("output lock").clear();
        // The count restarts with the buffer: `answer_cursor_queries` counts
        // matches in the (now empty) stream. Only this thread touches
        // `answered`, so it needs no place under the lock.
        self.answered = 0;
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("resize pty");
        self.rows = rows;
        self.cols = cols;
    }

    /// Is the app still running? A pty child that died mid-resize reads
    /// exactly like a hang, so tests assert this explicitly.
    pub fn is_alive(&mut self) -> bool {
        self.child.try_wait().expect("poll child").is_none()
    }

    /// Type `text` one byte at a time, the way a person does — a single write
    /// can arrive as one paste-shaped burst and take a different code path.
    pub fn type_text(&mut self, text: &str) {
        for byte in text.as_bytes() {
            self.press(&[*byte]);
            std::thread::sleep(Duration::from_millis(15));
        }
        // Let the last keystroke land before the caller sends Enter. Without
        // this the submit can overtake the final character and the command
        // dispatches short.
        std::thread::sleep(Duration::from_millis(400));
    }

    // ── Reading the screen ──────────────────────────────────────────────

    /// The terminal grid: exactly [`ROWS`] lines, trailing blanks trimmed.
    ///
    /// Parsed fresh from the whole byte stream each call. Re-parsing is cheap
    /// at test sizes and avoids sharing a parser with the reader thread.
    pub fn frame(&self) -> Vec<String> {
        let bytes = self.output.lock().expect("output lock").clone();
        let mut parser = vt100::Parser::new(self.rows, self.cols, 0);
        parser.process(&bytes);
        let screen = parser.screen();
        (0..self.rows)
            .map(|row| {
                let line = screen.contents_between(row, 0, row, self.cols);
                line.trim_end().to_string()
            })
            .collect()
    }

    /// The frame as one string, for assertion messages.
    pub fn frame_text(&self) -> String {
        self.frame().join("\n")
    }

    /// The two footer lines — the persistent mode band at the bottom.
    pub fn footer(&self) -> Vec<String> {
        let frame = self.frame();
        frame[frame.len().saturating_sub(2)..].to_vec()
    }

    /// Repaint, settle, and read. Every snapshot goes through this so a frame
    /// is never a half-applied diff.
    ///
    /// Settling is observed rather than assumed: poll until the grid stops
    /// changing for [`SETTLE_STABLE_POLLS`] consecutive reads, then snapshot.
    /// This replaced a flat 500ms + 200ms sleep, which was both slower than it
    /// needed to be on a fast machine and — the part that matters — a fixed bet
    /// that a loaded CI runner would finish painting inside 700ms. The cap is
    /// that same 700ms, so the slow case is no worse than before and the common
    /// case is ~150ms.
    fn settled_frame(&mut self) -> Vec<String> {
        self.press(CTRL_L);
        let deadline = Instant::now() + SETTLE_MAX;
        let mut previous = Vec::new();
        let mut stable: u8 = 0;
        loop {
            std::thread::sleep(SETTLE_POLL);
            // Keep answering cursor-position queries while we wait: the app can
            // be blocked on a reply, in which case the grid never changes until
            // we send one.
            let raw = self.raw();
            self.answer_cursor_queries(&raw);
            let frame = self.frame();
            stable = if frame == previous { stable + 1 } else { 0 };
            previous = frame;
            if stable >= SETTLE_STABLE_POLLS || Instant::now() >= deadline {
                return previous;
            }
        }
    }

    // ── Snapshots ───────────────────────────────────────────────────────

    /// Compare the whole settled frame against `tests/snapshots/<name>.txt`.
    pub fn assert_frame(&mut self, name: &str) {
        let actual = normalize(&self.settled_frame(), &self.sandbox);
        compare_snapshot(name, &actual);
    }

    /// Compare only the footer — for assertions that are about the mode band
    /// and should not churn when the transcript above it changes.
    pub fn assert_footer(&mut self, name: &str) {
        let _ = self.settled_frame();
        let actual = normalize(&self.footer(), &self.sandbox);
        compare_snapshot(name, &actual);
    }

    // ── Waiting ─────────────────────────────────────────────────────────

    /// Wait until `needle` appears on the grid. Whitespace-insensitive: ratatui
    /// jumps over unchanged cells, so a phrase can be split by a cursor move.
    pub fn wait_for_text(&mut self, needle: &str, timeout: Duration) -> bool {
        let squashed_needle = squash(needle);
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let raw = self.raw();
            self.answer_cursor_queries(&raw);
            if squash(&self.frame_text()).contains(&squashed_needle) {
                return true;
            }
            std::thread::sleep(WAIT_POLL);
        }
        false
    }

    /// Wait until `needle` is no longer on the grid.
    pub fn wait_for_gone(&mut self, needle: &str, timeout: Duration) -> bool {
        let squashed_needle = squash(needle);
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let raw = self.raw();
            self.answer_cursor_queries(&raw);
            if !squash(&self.frame_text()).contains(&squashed_needle) {
                return true;
            }
            std::thread::sleep(WAIT_POLL);
        }
        false
    }

    pub fn wait(&mut self, pred: impl Fn(&str) -> bool, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let raw = self.raw();
            self.answer_cursor_queries(&raw);
            if pred(&raw) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    fn raw(&self) -> String {
        let bytes = self.output.lock().expect("output lock").clone();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// Answer `ESC[6n` (Device Status Report) — see the module docs.
    fn answer_cursor_queries(&mut self, text: &str) {
        let seen = text.matches("\x1b[6n").count();
        while self.answered < seen {
            self.answered += 1;
            let _ = self.writer.write_all(b"\x1b[1;1R");
            let _ = self.writer.flush();
        }
    }
}

// ── Normalization ───────────────────────────────────────────────────────

/// Rewrite the handful of things that legitimately differ between machines and
/// runs. Everything NOT listed here is compared verbatim.
///
/// Each entry is a deliberate blind spot, so the list is kept short: a frame
/// cannot catch a regression in something it redacts.
pub fn normalize(frame: &[String], sandbox: &Path) -> String {
    // Longest-first so a nested path is replaced before its parent.
    let sandbox_str = sandbox.display().to_string();
    let sandbox_alt = sandbox_str.replace('\\', "/");

    let frame = drop_startup_notice(frame);
    let lines: Vec<String> = frame
        .iter()
        .map(|line| {
            let mut line = line.clone();
            for needle in [sandbox_str.as_str(), sandbox_alt.as_str()] {
                if !needle.is_empty() {
                    line = line.replace(needle, "<SANDBOX>");
                }
            }
            line = redact_version(&line);
            line = redact_cwd_line(&line);
            line = redact_clock(&line);
            line.trim_end().to_string()
        })
        .collect();
    collapse_blank_runs(&lines).join("\n")
}

/// Remove the startup web-capability notice.
///
/// It is environment-dependent *by design*: the text names the host triple, and
/// a healthy all-local web setup suppresses it entirely. Baking it into a
/// snapshot means the frame passes on the machine that generated it and fails
/// everywhere else — which is exactly what happened here, on a suite CI runs
/// across ubuntu, macOS and windows.
///
/// Dropped rather than placeholdered because its LINE COUNT varies too, and a
/// placeholder would still shift every row beneath it.
fn drop_startup_notice(frame: &[String]) -> Vec<String> {
    let Some(start) = frame
        .iter()
        .position(|line| line.contains("Web capabilities"))
    else {
        return frame.to_vec();
    };
    // The notice runs until the next blank row.
    let end = frame[start..]
        .iter()
        .position(|line| line.trim().is_empty())
        .map(|offset| start + offset)
        .unwrap_or(frame.len());
    let mut out = frame[..start].to_vec();
    out.extend_from_slice(&frame[end..]);
    out
}

/// Collapse runs of blank rows to one.
///
/// Vertical padding is a function of how many rows the content above happened
/// to occupy, so it moves with anything the frame deliberately does not assert
/// (the notice dropped above). Horizontal fidelity — where every glyph sits on
/// its row — is preserved in full, and that is where the bugs this suite exists
/// for actually live.
fn collapse_blank_runs(lines: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    for line in lines {
        if line.trim().is_empty() && out.last().is_some_and(|prev| prev.trim().is_empty()) {
            continue;
        }
        out.push(line.clone());
    }
    out
}

/// `mermaid v<semver>` → `mermaid v<VERSION>`; the footer prints it every frame.
fn redact_version(line: &str) -> String {
    let Some(start) = line.find("mermaid v") else {
        return line.to_string();
    };
    let rest = &line[start + "mermaid v".len()..];
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(rest.len());
    format!("{}mermaid v<VERSION>{}", &line[..start], &rest[end..])
}

/// The `user@host:cwd … context: …` line, replaced whole.
///
/// Not just the `user@host` prefix: the cwd is right-padded to push the context
/// gauge to the right edge, so the sandbox path's LENGTH decides where — and
/// whether — the gauge is clipped. Normalizing the prefix alone leaves that
/// length encoded in the snapshot, which would make it fail on a machine whose
/// temp directory is a different width.
fn redact_cwd_line(line: &str) -> String {
    let Some(at) = line.find('@') else {
        return line.to_string();
    };
    // Only the leading `user@host:` form, not an email inside prose.
    if at == 0 || line[..at].contains(' ') {
        return line.to_string();
    }
    "<USER>@<HOST>:<CWD>   <CONTEXT GAUGE>".to_string()
}

/// The right-aligned message timestamp → `<TIME>`.
///
/// It renders three ways — `Today at 7:00am`, `Yesterday at …`, and
/// `January 1st, 2026 at 7:00am` for anything older — and the absolute form is
/// LOCAL time, so a fixture stamped in UTC prints differently in every
/// timezone. Matching the whole right-aligned tail covers all three and makes
/// the snapshot timezone-independent.
fn redact_clock(line: &str) -> String {
    // The gutter right-aligns the stamp behind a run of padding.
    let Some(pad) = line.rfind("  ") else {
        return line.to_string();
    };
    let tail = line[pad..].trim();
    if !looks_like_timestamp(tail) {
        return line.to_string();
    }
    format!("{}  <TIME>", line[..pad].trim_end())
}

/// Does `text` end in a clock time and contain the ` at ` the renderer puts in
/// front of it? Deliberately narrow — anything looser would start eating real
/// content off the right edge of a frame.
fn looks_like_timestamp(text: &str) -> bool {
    let Some(time) = text.rsplit(" at ").next() else {
        return false;
    };
    if !text.contains(" at ") {
        return false;
    }
    let time = time.trim_end_matches(['a', 'p', 'm']);
    let mut halves = time.split(':');
    let (Some(hour), Some(minute), None) = (halves.next(), halves.next(), halves.next()) else {
        return false;
    };
    !hour.is_empty()
        && hour.chars().all(|c| c.is_ascii_digit())
        && minute.len() == 2
        && minute.chars().all(|c| c.is_ascii_digit())
}

fn squash(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

// ── Snapshot files ──────────────────────────────────────────────────────

fn snapshot_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("snapshots")
        .join(format!("{name}.txt"))
}

fn compare_snapshot(name: &str, actual: &str) {
    let path = snapshot_path(name);
    let updating = std::env::var_os("UPDATE_SNAPSHOTS").is_some();

    if updating {
        std::fs::create_dir_all(path.parent().expect("snapshot dir")).expect("create snapshot dir");
        std::fs::write(&path, format!("{actual}\n")).expect("write snapshot");
        eprintln!("updated snapshot {}", path.display());
        return;
    }

    // Normalize CRLF: the checkout's line endings are a property of the
    // developer's git config, not of what the terminal drew. Without this the
    // suite passes on the machine that generated the files and fails on a
    // Windows CI runner with `autocrlf=true`, where every expected row carries
    // a trailing CR the frame never had. `.gitattributes` pins these files to
    // LF too; this is the belt to that suspenders, since a stray checkout
    // setting must not be able to turn every frame red.
    let Ok(expected) = std::fs::read_to_string(&path).map(|text| text.replace("\r\n", "\n")) else {
        panic!(
            "missing snapshot {}\n\nRe-run with UPDATE_SNAPSHOTS=1 to create it, then review \
             the file before committing.\n\n--- actual ---\n{actual}",
            path.display(),
        );
    };
    let expected = expected.trim_end_matches('\n');
    if expected == actual {
        return;
    }
    panic!(
        "frame does not match {}\n\nIf the change is intended, re-run with UPDATE_SNAPSHOTS=1 \
         and review the diff.\n\n{}",
        path.display(),
        line_diff(expected, actual),
    );
}

/// A line-oriented diff, so a failure names the row that moved instead of
/// dumping two 30-line blocks and leaving the reader to spot it.
fn line_diff(expected: &str, actual: &str) -> String {
    let expected: Vec<&str> = expected.lines().collect();
    let actual: Vec<&str> = actual.lines().collect();
    let mut out = String::new();
    for row in 0..expected.len().max(actual.len()) {
        let e = expected.get(row).copied().unwrap_or("<missing>");
        let a = actual.get(row).copied().unwrap_or("<missing>");
        if e == a {
            continue;
        }
        out.push_str(&format!(
            "row {row}:\n  expected: {e:?}\n  actual:   {a:?}\n"
        ));
    }
    if out.is_empty() {
        out.push_str("(lines match; trailing-whitespace or line-count difference)\n");
    }
    out
}

// ── Sandbox ─────────────────────────────────────────────────────────────

pub fn test_sandbox(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("{tag}-{nanos}"));
    std::fs::create_dir_all(&dir).expect("create sandbox");
    dir
}

/// Write a two-message conversation `--resume` can load.
///
/// Built as raw JSON rather than through `ConversationHistory` so the fixture
/// states exactly what lands on disk — a serialization change that alters the
/// rendered frame should show up as a snapshot diff, not be absorbed silently.
fn write_conversation(path: &Path, id: &str, prompt: &str, reply: &str) {
    let stamp = "2026-01-01T12:00:00-00:00";
    let history = serde_json::json!({
        "id": id,
        "title": "pty frame fixture",
        "messages": [
            // `MessageRole` serializes with its Rust variant names ("User",
            // not "user"), and its deserializer maps anything it does not
            // recognize to `System` rather than failing. A lowercase fixture
            // therefore loads silently as system chatter — rendered as flat dim
            // text with no markdown — which is exactly what the first captured
            // frame showed.
            {
                "role": "User",
                "content": prompt,
                "timestamp": stamp,
                "kind": "normal",
            },
            {
                "role": "Assistant",
                "content": reply,
                "timestamp": stamp,
                "kind": "normal",
            },
        ],
        "model_name": TEST_MODEL,
        "project_path": path.parent().and_then(Path::parent).and_then(Path::parent)
            .map(|p| p.display().to_string()).unwrap_or_default(),
        "created_at": stamp,
        "updated_at": stamp,
    });
    std::fs::write(
        path,
        serde_json::to_string_pretty(&history).expect("serialize fixture"),
    )
    .expect("write conversation fixture");
}
