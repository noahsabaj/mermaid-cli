//! `$EDITOR` compose (Ctrl+O / `/editor`): suspend the TUI, open the user's
//! editor on the input draft, and hand the result back as a `Msg`.
//!
//! Owned by the run loop — not the effect runner — because the round-trip
//! must tear down and rebuild the terminal guard and crossterm event stream,
//! which only `run_interactive_with` holds. The suspend order is
//! load-bearing (see [`compose_in_editor`]).

use crate::app::terminal::TerminalGuard;
use crate::domain::Msg;
use anyhow::Result;
use crossterm::event::EventStream;
use std::path::{Path, PathBuf};

/// Resolve the compose editor: `$VISUAL` then `$EDITOR`, first non-blank
/// wins. The value is a shell fragment (it may carry flags like
/// `code --wait`), so it is passed to the shell unquoted.
fn resolve_editor() -> Option<String> {
    ["VISUAL", "EDITOR"].iter().find_map(|var| {
        std::env::var(var)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    })
}

/// Stage the draft in the private (0700) scratch dir with owner-only mode —
/// drafts can contain anything the user was about to send.
fn write_compose_file(text: &str) -> std::io::Result<PathBuf> {
    let dir = mermaid_model::utils::private_temp_dir()?;
    let path = dir.join(format!("compose-{}.md", std::process::id()));
    std::fs::write(&path, text)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(path)
}

/// Launch `<editor> <path>` through the shell with inherited stdio and wait.
/// Blocking by design — the caller parks it on `spawn_blocking`.
fn run_editor_blocking(editor: &str, path: &Path) -> std::io::Result<std::process::ExitStatus> {
    #[cfg(unix)]
    {
        // Single-quote the path (the editor fragment itself stays unquoted so
        // flags survive); a quote inside the path closes/reopens the quoting.
        let quoted = format!("'{}'", path.display().to_string().replace('\'', r"'\''"));
        std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("{editor} {quoted}"))
            .status()
    }
    #[cfg(windows)]
    {
        std::process::Command::new("cmd")
            .arg("/C")
            .arg(format!("{editor} \"{}\"", path.display()))
            .status()
    }
}

/// Read the edited draft back, dropping one trailing newline (editors append
/// one on save; the composer draft shouldn't grow a blank line per round-trip).
fn read_back(path: &Path) -> std::io::Result<String> {
    let bytes = std::fs::read(path)?;
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if text.ends_with('\n') {
        text.pop();
        if text.ends_with('\r') {
            text.pop();
        }
    }
    Ok(text)
}

/// Suspend the TUI, run the editor on `text`, and return the resulting `Msg`
/// (`EditorReturned` on success, `TransientStatus` on any failure).
///
/// Order is load-bearing:
/// 1. Resolve the editor and stage the temp file BEFORE touching the
///    terminal — a missing `$EDITOR` must not flicker the screen.
/// 2. Drop the `EventStream` (its reader thread holds crossterm's internal
///    reader mutex, which would swallow terminal queries).
/// 3. `restore_now()` (kitty Pop before LeaveAlternateScreen inside).
/// 4. Run the editor to completion on a blocking thread.
/// 5. `TerminalGuard::setup()` + a fresh `EventStream` — the setup-time
///    kitty probe is legal exactly because no stream exists yet. The fresh
///    ratatui terminal starts with an empty back buffer, so the next draw
///    repaints every cell; no explicit repaint needed.
///
/// Errors are fatal only when the terminal cannot be re-established.
pub(crate) async fn compose_in_editor(
    terminal: &mut Option<TerminalGuard>,
    events: &mut Option<EventStream>,
    text: String,
) -> Result<Msg> {
    let Some(editor) = resolve_editor() else {
        return Ok(Msg::TransientStatus {
            text: "No editor configured: set $VISUAL or $EDITOR.".to_string(),
        });
    };
    let path = match write_compose_file(&text) {
        Ok(p) => p,
        Err(err) => {
            return Ok(Msg::TransientStatus {
                text: format!("Could not stage the compose file: {err}"),
            });
        },
    };

    events.take();
    if let Some(mut guard) = terminal.take() {
        guard.restore_now();
    }

    let editor_for_task = editor.clone();
    let path_for_task = path.clone();
    let status =
        tokio::task::spawn_blocking(move || run_editor_blocking(&editor_for_task, &path_for_task))
            .await;

    // Re-enter the TUI before inspecting the outcome, so even a failed editor
    // leaves a working screen. A setup failure here is fatal for the loop.
    *terminal = Some(TerminalGuard::setup()?);
    *events = Some(EventStream::new());

    let msg = match status {
        Ok(Ok(st)) if st.success() => match read_back(&path) {
            Ok(text) => Msg::EditorReturned { text: Some(text) },
            Err(err) => Msg::TransientStatus {
                text: format!("Editor exited but the draft could not be read back: {err}"),
            },
        },
        Ok(Ok(st)) => Msg::TransientStatus {
            text: format!("Editor exited with {st}; draft left unchanged."),
        },
        Ok(Err(err)) => Msg::TransientStatus {
            text: format!("Could not launch '{editor}': {err}"),
        },
        Err(err) => Msg::TransientStatus {
            text: format!("Editor task failed: {err}"),
        },
    };
    let _ = std::fs::remove_file(&path);
    Ok(msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_back_strips_one_trailing_newline() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("mermaid-editor-readback-{}", std::process::id()));
        std::fs::write(&path, "draft body\n").unwrap();
        assert_eq!(read_back(&path).unwrap(), "draft body");
        std::fs::write(&path, "keep\n\n").unwrap();
        assert_eq!(read_back(&path).unwrap(), "keep\n");
        std::fs::write(&path, "crlf\r\n").unwrap();
        assert_eq!(read_back(&path).unwrap(), "crlf");
        std::fs::write(&path, "").unwrap();
        assert_eq!(read_back(&path).unwrap(), "");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn compose_file_is_owner_only() {
        let path = write_compose_file("secret draft").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "secret draft");
        let _ = std::fs::remove_file(&path);
    }
}
