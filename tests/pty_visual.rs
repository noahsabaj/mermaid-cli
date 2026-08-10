//! Behavioral regressions driven through a real pty — what the user actually
//! sees, asserted as *behavior* rather than as a whole frame.
//!
//! Unit tests assert on `Line`/`Span` values, which is why the footer could
//! ship a mode band nobody had looked at (`plan mode on (alt+p to toggle) -
//! restores: auto`) — every assertion passed, and the string was still wrong.
//! This drives the actual binary the way a user does: spawn it on a pty, press
//! keys, force a full repaint, and read the glyphs that landed on the screen.
//!
//! The sibling file `pty_frame.rs` snapshots entire frames. Use that for
//! layout; use this for a specific claim about what a key does, where naming
//! the claim is worth more than pinning every cell around it.
//!
//! Cross-platform on purpose (openpty on Unix, ConPTY on Windows).

mod harness;

use harness::{CTRL_L, ENTER, ESC, SHIFT_TAB, Terminal};
use std::time::Duration;

/// The safety cycle is flat and total: five modes, `plan` included, wrapping
/// back to where it started. `plan` is a mode here — not a badge layered on one.
#[test]
fn shift_tab_cycles_every_safety_mode_in_the_footer() {
    let mut term = Terminal::launch("pty-safety");

    // Starts in `ask` (the default), so the cycle order from here is fixed.
    for expected in ["auto", "full_access", "plan", "read_only", "ask"] {
        term.press(SHIFT_TAB);
        assert!(
            term.wait_for_text(&format!("safety: {expected}"), Duration::from_secs(10)),
            "Shift+Tab should have reached {expected}. Screen:\n{}",
            term.frame_text(),
        );
    }

    // The retired plan band must not come back in any form.
    let screen = term.frame_text().to_lowercase();
    assert!(
        !screen.contains("restores:"),
        "the footer still names a restore target:\n{}",
        term.frame_text()
    );
    assert!(
        !screen.contains("alt+p"),
        "the footer still advertises Alt+P:\n{}",
        term.frame_text()
    );
}

/// End-to-end proof for the screenshot-paste bug: a clipboard carrying `PNG`
/// with no `CF_BITMAP` — what Snipping Tool, Win+Shift+S, Chrome and Figma
/// produce — used to make `Clipboard::ContainsImage()` answer False, so Ctrl+V
/// fell through to a text read and nothing was attached. Pressing the key in
/// the real app must splice an `[Image #1]` token into the input.
#[cfg(windows)]
#[test]
fn ctrl_v_attaches_a_png_only_clipboard() {
    const CTRL_V: &[u8] = &[0x16];

    let png = std::env::temp_dir().join("mermaid-pty-visual-probe.png");
    // Arm the exact failing shape: build a bitmap, save it as a PNG, then put
    // ONLY the encoded bytes on the clipboard under the `PNG` format.
    let arm = format!(
        "\
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
$bmp = New-Object System.Drawing.Bitmap 120, 80
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.Clear([System.Drawing.Color]::Teal)
$g.Dispose()
$bmp.Save('{png}', [System.Drawing.Imaging.ImageFormat]::Png)
$ms = New-Object System.IO.MemoryStream(,[System.IO.File]::ReadAllBytes('{png}'))
$d = New-Object System.Windows.Forms.DataObject
$d.SetData('PNG', $false, $ms)
[System.Windows.Forms.Clipboard]::SetDataObject($d, $true)
if ([System.Windows.Forms.Clipboard]::ContainsImage()) {{ throw 'expected a PNG-only clipboard' }}",
        png = png.display(),
    );
    let armed = std::process::Command::new("powershell")
        .args(["-NoProfile", "-STA", "-Command", &arm])
        .output()
        .expect("run the clipboard arming script");
    assert!(
        armed.status.success(),
        "could not arm a PNG-only clipboard: {}",
        String::from_utf8_lossy(&armed.stderr)
    );

    let mut term = Terminal::launch("pty-paste");
    term.press(CTRL_V);
    // The paste shells out to PowerShell twice (probe, then read) and pays CLR
    // startup on a cold host, so give it room.
    let landed = term.wait_for_text("[Image #1]", Duration::from_secs(30));
    let _ = std::fs::remove_file(&png);
    assert!(
        landed,
        "Ctrl+V did not attach the image. Screen:\n{}",
        term.frame_text()
    );
}

/// A multiline paste lands in the composer as ONE insert — no per-line
/// submits, no model calls.
///
/// ConPTY delivers a terminal paste as several key-event chunks (crossterm
/// emits no `Event::Paste` on the Windows console), and a chunk gap landing
/// right after an Enter made that Enter a lone key — a submit. Pasting a
/// three-line prompt fired two half-prompts at the model (issue #351). On
/// unix the same bytes parse as a real bracketed paste, so this claim holds
/// trivially there and guards the chunk bridge on Windows.
#[test]
fn a_multiline_paste_never_submits() {
    let mut term = Terminal::launch("pty-paste-chunks");
    term.press(b"\x1b[200~line one\r\nline two\nline three\x1b[201~");
    assert!(
        term.wait_for_text("line three", Duration::from_secs(10)),
        "the paste never reached the composer. Screen:\n{}",
        term.frame_text(),
    );
    // A wrongly-submitted line fires a model call whose failure renders in
    // the transcript; give one time to surface before reading the frame.
    std::thread::sleep(Duration::from_millis(1200));
    term.press(CTRL_L);
    std::thread::sleep(Duration::from_millis(300));
    let screen = term.frame_text();
    assert!(
        !screen.contains("Auth error") && !screen.contains("Worked for"),
        "a pasted line was submitted as a message. Screen:\n{screen}",
    );
    assert!(
        screen.contains("line one") && screen.contains("line two"),
        "the whole paste must sit in the composer. Screen:\n{screen}",
    );
}

/// Shrinking the terminal keeps the composer and the mode band on screen.
///
/// The chat zone's 10-row floor (`Constraint::Min`) outranks the fixed
/// `Length` zones below it in the layout solver, so at 10 rows the
/// transcript consumed the whole frame — no input box, no footer. The two
/// surfaces the user actually drives were exactly the ones that vanished,
/// silently: typing still worked, invisibly. A short terminal must keep
/// the composer and footer; the transcript absorbs the shortage.
#[test]
fn a_short_terminal_still_shows_the_composer_and_footer() {
    let mut term = Terminal::launch("pty-short");
    term.press(b"draft text");
    assert!(
        term.wait_for_text("draft text", Duration::from_secs(10)),
        "typed text must reach the composer before the resize. Screen:\n{}",
        term.frame_text(),
    );

    term.resize(10, 60);
    term.press(CTRL_L);
    assert!(term.is_alive(), "the app must survive the resize");
    assert!(
        term.wait_for_text("safety:", Duration::from_secs(10)),
        "the footer mode band must stay on a 10-row screen. Screen:\n{}",
        term.frame_text(),
    );
    assert!(
        term.wait_for_text("draft text", Duration::from_secs(10)),
        "the composer draft must stay on a 10-row screen. Screen:\n{}",
        term.frame_text(),
    );
}

/// `/model` opens a picker rather than printing the current model, and Esc
/// dismisses it without disturbing the composer.
#[test]
fn slash_model_opens_a_picker_and_escape_dismisses_it() {
    let mut term = Terminal::launch("pty-model");
    term.type_text("/model");
    term.press(ENTER);

    assert!(
        term.wait_for_text("Select model", Duration::from_secs(20)),
        "/model did not open a picker. Screen:\n{}",
        term.frame_text()
    );
    for hint in ["Enter switch", "type to filter", "Esc cancel"] {
        assert!(
            term.wait_for_text(hint, Duration::from_secs(5)),
            "the picker must advertise {hint:?}. Screen:\n{}",
            term.frame_text()
        );
    }

    term.press(ESC);
    assert!(
        term.wait_for_gone("Select model", Duration::from_secs(5)),
        "Esc did not dismiss the picker. Screen:\n{}",
        term.frame_text()
    );
}

/// The picker enumerated only Ollama and the OpenAI-compatible registry, so a
/// bespoke provider's models — Meta's `muse-spark-*` among them — never showed
/// up no matter how many turns the user had run on one. Typing the provider
/// name must narrow to real rows from its catalog.
///
/// Listing a catalog costs no tokens, but it does need a key, so this skips
/// (loudly) without one.
#[test]
fn the_picker_lists_a_bespoke_providers_models() {
    if std::env::var("MODEL_API_KEY").is_err() {
        eprintln!("skipping: MODEL_API_KEY is not set");
        return;
    }
    let mut term = Terminal::launch("pty-model-meta");
    term.type_text("/model");
    term.press(ENTER);
    assert!(
        term.wait_for_text("Select model", Duration::from_secs(20)),
        "/model did not open a picker. Screen:\n{}",
        term.frame_text()
    );

    // Filter to the provider so its rows survive the pane's window regardless
    // of how many local models sit above them.
    term.type_text("meta/");
    assert!(
        term.wait_for_text("meta/muse-spark", Duration::from_secs(30)),
        "the picker never listed a meta model. Screen:\n{}",
        term.frame_text()
    );
}
