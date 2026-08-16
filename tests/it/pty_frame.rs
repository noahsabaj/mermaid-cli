//! Golden-frame regressions: the whole rendered screen, compared cell by cell.
//!
//! # Why a frame and not an assertion
//!
//! Three separate bugs shipped in one session — inline code rendered as a row
//! of disconnected boxes, a modal clipped mid-word at its border, a status band
//! advertising a keybinding that no longer existed — and every unit test passed
//! through all of them. They assert on `Line`/`Span` *values*, which cannot see
//! a shredded background, a glyph past column 78, or a sentence nobody reread.
//!
//! A golden frame is the missing detector: it captures what the terminal
//! actually painted, so any visual change has to be looked at and accepted by a
//! human before it lands.
//!
//! # How it stays deterministic
//!
//! Two problems have to be solved or the snapshots are useless.
//!
//! **Cells, not bytes.** Ratatui redraws only the cells that changed and jumps
//! over the rest with cursor moves, so the raw pty byte stream is not the
//! screen — a rendered `[Image #1] ` arrives split as `[Image` … `#1]`. The
//! bytes are fed to a real VT parser and the *grid* is snapshotted.
//!
//! **Volatile content is redacted, not banned.** A frame legitimately contains
//! a version, a sandbox path with a nanosecond timestamp, a hostname, and a
//! clock. [`normalize`] rewrites exactly those to placeholders. Everything else
//! is compared verbatim — the redaction list is deliberately short and explicit,
//! because each entry is a thing the snapshot can no longer catch.
//!
//! What is deliberately NOT snapshotted: the startup web-capability notice
//! (its text names the host triple and a healthy local setup suppresses it
//! entirely) and vertical blank padding (which moves with it). There is no
//! whole-startup-screen frame for the same reason — it would assert almost
//! nothing this project controls and a great deal it does not.
//!
//! Run with `UPDATE_SNAPSHOTS=1` to (re)write `tests/snapshots/*.txt`. Review
//! the diff like any other diff: an unexplained change is the bug.

use crate::harness::{self, ENTER, Terminal};
use std::time::Duration;

/// Assistant markdown as the transcript actually paints it: multi-word inline
/// code, a wrapped bullet list, and a table.
///
/// This is the frame that would have caught the shredded-inline-code bug. The
/// transcript is seeded on disk and loaded with `--resume`, so no model is
/// called and the content is fixed forever.
#[test]
fn assistant_markdown_frame() {
    const BODY: &str = "\
Clipboard paste is wired for every backend, with prerequisites:

- **Windows** — `Clipboard::ContainsImage()` misses a PNG-only clipboard, so \
`No image data found in clipboard` was the result of every screenshot paste.
- **Linux** — needs `wl-clipboard` on Wayland or `xclip` on X11, plus \
`WAYLAND_DISPLAY`/`DISPLAY` set.

| OS | Backend | Guard |
|---|---|---|
| Windows | PowerShell | 10s |
| macOS | pngpaste | 5s |
";

    let mut term =
        Terminal::launch_with_transcript("frame-markdown", "read me the clipboard notes", BODY);
    term.assert_frame("assistant_markdown");
}

/// The `/model` picker pane, filtered to no matches.
///
/// Which models exist is a property of the developer's machine — installed
/// Ollama models, provider keys — so a populated list can never be a stable
/// snapshot. Filtering to a string nothing matches makes the frame
/// deterministic while still pinning what actually broke historically: the
/// border, the title, the hint row, and the filter footer. That the picker
/// *does* list real models is asserted behaviorally in `pty_visual.rs`.
#[test]
fn model_picker_frame() {
    let mut term = Terminal::launch("frame-model-picker");
    term.type_text("/model");
    term.press(ENTER);
    assert!(
        term.wait_for_text("Select model", Duration::from_secs(25)),
        "picker never opened:\n{}",
        term.frame_text()
    );
    // Discovery must FINISH before typing. Waiting on "filter:" was not enough:
    // that row is present while loading too, so the filter keystrokes raced the
    // arrival of the model list and only some of them landed in the pane.
    assert!(
        term.wait_for_gone("still searching", Duration::from_secs(30)),
        "model discovery never settled:\n{}",
        term.frame_text()
    );
    term.type_text("zzzznomatch");
    assert!(
        term.wait_for_text("Nothing matches", Duration::from_secs(10)),
        "filter did not empty the list:\n{}",
        term.frame_text()
    );
    term.assert_frame("model_picker_filtered");
}

/// Every safety mode's footer, in one frame per mode. Plan is a cycle position
/// like the rest — the band that used to read
/// `plan mode on (alt+p to toggle) - restores: <mode>` is gone, and a golden
/// frame is what makes its return impossible to miss.
#[test]
fn safety_mode_footers() {
    let mut term = Terminal::launch("frame-safety");
    for mode in ["auto", "full_access", "plan", "read_only", "ask"] {
        term.press(harness::SHIFT_TAB);
        term.wait_for_text(&format!("safety: {mode}"), Duration::from_secs(10));
        term.assert_footer(&format!("footer_{mode}"));
    }
}
