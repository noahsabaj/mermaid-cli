//! Clipboard access for image and text paste
//!
//! Auto-detects the platform and display server, then uses the appropriate
//! system tool to read clipboard contents:
//! - Linux/Wayland: wl-paste
//! - Linux/X11: xclip
//! - macOS: pbpaste / osascript (for images)
//! - Windows: PowerShell Get-Clipboard
//!
//! Every one of those tools can hang: the X11/Wayland clipboard is served *by
//! the application that owns the selection*, so a frozen owner — or a stale
//! `$DISPLAY`/`$WAYLAND_DISPLAY` pointing at a dead server — blocks a read
//! forever, and PowerShell can wedge on a broken CLR. Nothing here calls
//! `Command::output()`/`wait()` directly; every subprocess runs under a
//! kill-on-timeout deadline so a wedged helper costs a bounded stall plus a
//! visible error, not a paste that silently never lands and a permanently
//! leaked blocking thread.

use anyhow::{Context, Result};
use std::process::Command;
use std::time::Duration;

use crate::utils::{output_with_timeout, write_stdin_with_timeout};

/// `which` existence probes and clipboard *metadata* queries (offered MIME
/// types, `osascript` clipboard info) — tiny payloads, so a slow answer means
/// the display server or selection owner is wedged, not that data is big.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Actual clipboard payload transfer (text or image bytes) — generous enough
/// for a multi-megabyte screenshot from a healthy owner, short enough that a
/// hung one can't wedge the paste path.
const DATA_TIMEOUT: Duration = Duration::from_secs(5);

/// PowerShell invocations pay CLR/JIT startup (seconds when cold) before any
/// clipboard work happens, so Windows gets a fatter budget.
const POWERSHELL_TIMEOUT: Duration = Duration::from_secs(10);

/// Display server / platform type
#[derive(Debug, Clone, Copy)]
enum ClipboardBackend {
    Wayland,
    X11,
    MacOS,
    Windows,
}

/// True if `name` resolves on PATH. Even this probe is deadline-bounded: a
/// PATH entry on dead NFS can wedge `which` itself.
fn tool_exists(name: &str) -> bool {
    output_with_timeout(Command::new("which").arg(name), PROBE_TIMEOUT)
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Detect the active clipboard backend
fn detect_backend() -> Option<ClipboardBackend> {
    // macOS
    if cfg!(target_os = "macos") && tool_exists("pbpaste") {
        return Some(ClipboardBackend::MacOS);
    }

    // Windows
    if cfg!(target_os = "windows") {
        return Some(ClipboardBackend::Windows);
    }

    // Linux: check Wayland first
    if std::env::var("WAYLAND_DISPLAY").is_ok() && tool_exists("wl-paste") {
        return Some(ClipboardBackend::Wayland);
    }

    // Linux: fall back to X11
    if std::env::var("DISPLAY").is_ok() && tool_exists("xclip") {
        return Some(ClipboardBackend::X11);
    }

    None
}

/// Check if the clipboard contains image data
pub fn has_image() -> bool {
    match detect_backend() {
        Some(ClipboardBackend::Wayland) => {
            output_with_timeout(Command::new("wl-paste").arg("--list-types"), PROBE_TIMEOUT)
                .map(|o| {
                    let types = String::from_utf8_lossy(&o.stdout);
                    types.contains("image/png") || types.contains("image/jpeg")
                })
                .unwrap_or(false)
        },
        Some(ClipboardBackend::X11) => output_with_timeout(
            Command::new("xclip").args(["-selection", "clipboard", "-t", "TARGETS", "-o"]),
            PROBE_TIMEOUT,
        )
        .map(|o| {
            let types = String::from_utf8_lossy(&o.stdout);
            types.contains("image/png") || types.contains("image/jpeg")
        })
        .unwrap_or(false),
        Some(ClipboardBackend::MacOS) => {
            // Check clipboard type via AppleScript
            output_with_timeout(
                Command::new("osascript").args(["-e", "clipboard info"]),
                PROBE_TIMEOUT,
            )
            .map(|o| {
                let info = String::from_utf8_lossy(&o.stdout);
                info.contains("PNGf") || info.contains("JPEG") || info.contains("TIFF")
            })
            .unwrap_or(false)
        },
        Some(ClipboardBackend::Windows) => {
            // PowerShell: check if clipboard contains an image.
            // `Add-Type` is required on PowerShell 7 (Core) and locked-down
            // environments where System.Windows.Forms isn't auto-loaded.
            // Matches the pattern used in read_image_bytes below.
            output_with_timeout(
                Command::new("powershell").args([
                    "-NoProfile",
                    "-Command",
                    "Add-Type -AssemblyName System.Windows.Forms; \
                     [System.Windows.Forms.Clipboard]::ContainsImage()",
                ]),
                POWERSHELL_TIMEOUT,
            )
            .map(|o| {
                let out = String::from_utf8_lossy(&o.stdout);
                out.trim() == "True"
            })
            .unwrap_or(false)
        },
        None => false,
    }
}

/// Read image bytes from the clipboard.
/// Returns (bytes, format) where format is "png" or "jpeg".
pub fn read_image_bytes() -> Result<(Vec<u8>, String)> {
    let backend = detect_backend()
        .context("No clipboard backend detected (need xclip, wl-paste, pbpaste, or PowerShell)")?;

    match backend {
        ClipboardBackend::Wayland | ClipboardBackend::X11 => {
            // Try PNG first, then JPEG
            for (mime, format) in [("image/png", "png"), ("image/jpeg", "jpeg")] {
                let output = match backend {
                    ClipboardBackend::Wayland => output_with_timeout(
                        Command::new("wl-paste").args(["--type", mime]),
                        DATA_TIMEOUT,
                    ),
                    ClipboardBackend::X11 => output_with_timeout(
                        Command::new("xclip").args(["-selection", "clipboard", "-t", mime, "-o"]),
                        DATA_TIMEOUT,
                    ),
                    _ => unreachable!(),
                };

                if let Ok(output) = output
                    && output.status.success()
                    && !output.stdout.is_empty()
                {
                    return Ok((output.stdout, format.to_string()));
                }
            }
            anyhow::bail!("No image data found in clipboard")
        },
        ClipboardBackend::MacOS => {
            // Use osascript to save clipboard image to a temp file, then read it
            // 0700 per-user scratch dir, not a world-readable shared /tmp path
            // another local user could read or pre-create/symlink (#11).
            let temp_path = crate::utils::private_temp_dir()?.join("mermaid-clipboard-paste.png");
            let temp_str = temp_path.to_string_lossy();
            let script = format!(
                "set theFile to POSIX file \"{}\"\n\
                 tell application \"System Events\" to set theData to the clipboard as «class PNGf»\n\
                 set fp to open for access theFile with write permission\n\
                 write theData to fp\n\
                 close access fp",
                temp_str
            );
            // Try the simpler pngpaste approach first (if available), fall back to osascript
            let pngpaste_output =
                output_with_timeout(Command::new("pngpaste").arg(&temp_path), DATA_TIMEOUT);
            let success = if let Ok(output) = pngpaste_output
                && output.status.success()
            {
                true
            } else {
                // Fall back to osascript
                output_with_timeout(
                    Command::new("osascript").args(["-e", &script]),
                    DATA_TIMEOUT,
                )
                .map(|o| o.status.success())
                .unwrap_or(false)
            };

            if success {
                let bytes = std::fs::read(&temp_path)
                    .context("Failed to read clipboard image from temp file")?;
                let _ = std::fs::remove_file(&temp_path);
                if !bytes.is_empty() {
                    return Ok((bytes, "png".to_string()));
                }
            }
            anyhow::bail!("No image data found in clipboard (macOS)")
        },
        ClipboardBackend::Windows => {
            // Use PowerShell to save clipboard image to temp file
            // 0700 per-user scratch dir, not a world-readable shared /tmp path
            // another local user could read or pre-create/symlink (#11).
            let temp_path = crate::utils::private_temp_dir()?.join("mermaid-clipboard-paste.png");
            let temp_str = temp_path.to_string_lossy();
            let script = format!(
                "Add-Type -AssemblyName System.Windows.Forms; \
                 $img = [System.Windows.Forms.Clipboard]::GetImage(); \
                 if ($img) {{ $img.Save('{}', [System.Drawing.Imaging.ImageFormat]::Png) }}",
                temp_str
            );
            let output = output_with_timeout(
                Command::new("powershell").args(["-NoProfile", "-Command", &script]),
                POWERSHELL_TIMEOUT,
            );

            if let Ok(output) = output
                && output.status.success()
                && temp_path.exists()
            {
                let bytes = std::fs::read(&temp_path)
                    .context("Failed to read clipboard image from temp file")?;
                let _ = std::fs::remove_file(&temp_path);
                if !bytes.is_empty() {
                    return Ok((bytes, "png".to_string()));
                }
            }
            anyhow::bail!("No image data found in clipboard (Windows)")
        },
    }
}

/// Read text from the clipboard (fallback when no image is found).
pub fn read_text() -> Result<String> {
    let backend = detect_backend()
        .context("No clipboard backend detected (need xclip, wl-paste, pbpaste, or PowerShell)")?;

    let output = match backend {
        ClipboardBackend::Wayland => output_with_timeout(
            Command::new("wl-paste").args(["--type", "text/plain"]),
            DATA_TIMEOUT,
        ),
        ClipboardBackend::X11 => output_with_timeout(
            Command::new("xclip").args(["-selection", "clipboard", "-o"]),
            DATA_TIMEOUT,
        ),
        ClipboardBackend::MacOS => output_with_timeout(&mut Command::new("pbpaste"), DATA_TIMEOUT),
        ClipboardBackend::Windows => output_with_timeout(
            Command::new("powershell").args(["-NoProfile", "-Command", "Get-Clipboard"]),
            POWERSHELL_TIMEOUT,
        ),
    };

    let output = output.context("Failed to execute clipboard command")?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        anyhow::bail!("Clipboard does not contain text")
    }
}

/// Write `text` to the system clipboard. Mirrors `read_text`'s backend
/// detection and shells out to the platform tool (no extra dependency):
/// `wl-copy` / `xclip` / `pbcopy` / PowerShell `Set-Clipboard`. Used by the
/// in-app drag-select copy path.
pub fn write_text(text: &str) -> Result<()> {
    let backend =
        detect_backend().context("No clipboard backend detected (need xclip/wl-copy/pbcopy)")?;

    let (mut cmd, timeout) = match backend {
        ClipboardBackend::Wayland => (Command::new("wl-copy"), DATA_TIMEOUT),
        ClipboardBackend::X11 => {
            let mut cmd = Command::new("xclip");
            cmd.args(["-selection", "clipboard"]);
            (cmd, DATA_TIMEOUT)
        },
        ClipboardBackend::MacOS => (Command::new("pbcopy"), DATA_TIMEOUT),
        // Read all of stdin as UTF-8 and set the clipboard, so non-ASCII
        // survives (plain `clip.exe` reinterprets via the console codepage).
        ClipboardBackend::Windows => {
            let mut cmd = Command::new("powershell");
            cmd.args([
                "-NoProfile",
                "-Command",
                "[Console]::InputEncoding=[System.Text.Encoding]::UTF8; \
                 Set-Clipboard -Value ([Console]::In.ReadToEnd())",
            ]);
            (cmd, POWERSHELL_TIMEOUT)
        },
    };

    // `wl-copy` and `xclip` fork a background process that keeps *serving*
    // the selection after the parent exits; the helper points stdout/stderr
    // at null so that long-lived fork can't pin any pipe of ours.
    let status = write_stdin_with_timeout(&mut cmd, text.as_bytes().to_vec(), timeout)
        .context("clipboard write command failed to run")?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("clipboard write command exited with {status}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_backend() {
        // Just verify it doesn't panic — actual result depends on environment
        let _ = detect_backend();
    }

    #[test]
    fn test_has_image_no_crash() {
        // Should return false gracefully if no display server
        let _ = has_image();
    }

    /// Manual QA for a real display server (CI has none): round-trips a
    /// string through the system clipboard, then restores the previous text
    /// contents. Run with:
    /// `cargo test manual_clipboard_roundtrip -- --ignored --nocapture`
    #[test]
    #[ignore = "needs a real display server + clipboard tools"]
    fn manual_clipboard_roundtrip() {
        if detect_backend().is_none() {
            eprintln!("no clipboard backend detected; nothing to exercise");
            return;
        }
        let previous = read_text().ok();
        let probe = "mermaid clipboard self-test";
        write_text(probe).expect("write_text");
        // Selection serving is asynchronous on Wayland/X11 — give the
        // background fork a beat to take ownership.
        std::thread::sleep(Duration::from_millis(200));
        let read_back = read_text().expect("read_text");
        if let Some(prev) = previous {
            let _ = write_text(&prev);
        }
        // Tools may append a trailing newline (wl-paste does by default).
        assert_eq!(read_back.trim_end(), probe);
    }

    /// Manual QA for the failure mode this module guards against: a frozen
    /// selection owner (SIGSTOP'd `wl-copy --foreground`) must surface as a
    /// bounded timeout error, not a read that never returns. Wayland-only;
    /// briefly replaces the clipboard, restoring text contents afterwards.
    /// Run with:
    /// `cargo test manual_hung_owner_times_out -- --ignored --nocapture`
    #[cfg(unix)]
    #[test]
    #[ignore = "needs Wayland + wl-copy; simulates a frozen selection owner"]
    fn manual_hung_owner_times_out() {
        if std::env::var("WAYLAND_DISPLAY").is_err() || !tool_exists("wl-copy") {
            eprintln!("no Wayland session; nothing to exercise");
            return;
        }
        let previous = read_text().ok();

        // A foreground wl-copy serves the selection itself; SIGSTOP freezes
        // it mid-service so any paste request blocks forever.
        let mut owner = Command::new("wl-copy")
            .args(["--foreground", "hung-owner-data"])
            .spawn()
            .expect("spawn wl-copy");
        std::thread::sleep(Duration::from_millis(300));
        let stop = Command::new("kill")
            .args(["-STOP", &owner.id().to_string()])
            .status()
            .expect("SIGSTOP owner");
        assert!(stop.success());

        let start = std::time::Instant::now();
        let result = read_text();
        let elapsed = start.elapsed();

        // Unfreeze and clean up the owner before asserting, so a failure
        // doesn't leave a stopped process owning the user's clipboard.
        let _ = Command::new("kill")
            .args(["-CONT", &owner.id().to_string()])
            .status();
        let _ = owner.kill();
        let _ = owner.wait();
        if let Some(prev) = previous {
            let _ = write_text(&prev);
        }

        eprintln!("read_text against frozen owner: {result:?} after {elapsed:?}");
        assert!(
            result.is_err(),
            "a frozen selection owner must surface as an error"
        );
        assert!(
            elapsed < Duration::from_secs(15),
            "the deadline must bound the stall (took {elapsed:?})"
        );
    }
}
