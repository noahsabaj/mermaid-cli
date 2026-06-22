//! Clipboard access for image and text paste
//!
//! Auto-detects the platform and display server, then uses the appropriate
//! system tool to read clipboard contents:
//! - Linux/Wayland: wl-paste
//! - Linux/X11: xclip
//! - macOS: pbpaste / osascript (for images)
//! - Windows: PowerShell Get-Clipboard

use anyhow::{Context, Result};
use std::process::Command;

/// Display server / platform type
#[derive(Debug, Clone, Copy)]
enum ClipboardBackend {
    Wayland,
    X11,
    MacOS,
    Windows,
}

/// Detect the active clipboard backend
fn detect_backend() -> Option<ClipboardBackend> {
    // macOS
    if cfg!(target_os = "macos")
        && Command::new("which")
            .arg("pbpaste")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    {
        return Some(ClipboardBackend::MacOS);
    }

    // Windows
    if cfg!(target_os = "windows") {
        return Some(ClipboardBackend::Windows);
    }

    // Linux: check Wayland first
    if std::env::var("WAYLAND_DISPLAY").is_ok()
        && Command::new("which")
            .arg("wl-paste")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    {
        return Some(ClipboardBackend::Wayland);
    }

    // Linux: fall back to X11
    if std::env::var("DISPLAY").is_ok()
        && Command::new("which")
            .arg("xclip")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    {
        return Some(ClipboardBackend::X11);
    }

    None
}

/// Check if the clipboard contains image data
pub fn has_image() -> bool {
    match detect_backend() {
        Some(ClipboardBackend::Wayland) => Command::new("wl-paste")
            .arg("--list-types")
            .output()
            .map(|o| {
                let types = String::from_utf8_lossy(&o.stdout);
                types.contains("image/png") || types.contains("image/jpeg")
            })
            .unwrap_or(false),
        Some(ClipboardBackend::X11) => Command::new("xclip")
            .args(["-selection", "clipboard", "-t", "TARGETS", "-o"])
            .output()
            .map(|o| {
                let types = String::from_utf8_lossy(&o.stdout);
                types.contains("image/png") || types.contains("image/jpeg")
            })
            .unwrap_or(false),
        Some(ClipboardBackend::MacOS) => {
            // Check clipboard type via AppleScript
            Command::new("osascript")
                .args(["-e", "clipboard info"])
                .output()
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
            Command::new("powershell")
                .args([
                    "-NoProfile",
                    "-Command",
                    "Add-Type -AssemblyName System.Windows.Forms; \
                     [System.Windows.Forms.Clipboard]::ContainsImage()",
                ])
                .output()
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
                    ClipboardBackend::Wayland => {
                        Command::new("wl-paste").args(["--type", mime]).output()
                    },
                    ClipboardBackend::X11 => Command::new("xclip")
                        .args(["-selection", "clipboard", "-t", mime, "-o"])
                        .output(),
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
            let temp_path = std::env::temp_dir().join("mermaid-clipboard-paste.png");
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
            let pngpaste_output = Command::new("pngpaste").arg(&temp_path).output();
            let success = if let Ok(output) = pngpaste_output
                && output.status.success()
            {
                true
            } else {
                // Fall back to osascript
                Command::new("osascript")
                    .args(["-e", &script])
                    .output()
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
            let temp_path = std::env::temp_dir().join("mermaid-clipboard-paste.png");
            let temp_str = temp_path.to_string_lossy();
            let script = format!(
                "Add-Type -AssemblyName System.Windows.Forms; \
                 $img = [System.Windows.Forms.Clipboard]::GetImage(); \
                 if ($img) {{ $img.Save('{}', [System.Drawing.Imaging.ImageFormat]::Png) }}",
                temp_str
            );
            let output = Command::new("powershell")
                .args(["-NoProfile", "-Command", &script])
                .output();

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
        ClipboardBackend::Wayland => Command::new("wl-paste")
            .args(["--type", "text/plain"])
            .output(),
        ClipboardBackend::X11 => Command::new("xclip")
            .args(["-selection", "clipboard", "-o"])
            .output(),
        ClipboardBackend::MacOS => Command::new("pbpaste").output(),
        ClipboardBackend::Windows => Command::new("powershell")
            .args(["-NoProfile", "-Command", "Get-Clipboard"])
            .output(),
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
    use std::io::Write;
    use std::process::Stdio;

    let backend =
        detect_backend().context("No clipboard backend detected (need xclip/wl-copy/pbcopy)")?;

    let mut child = match backend {
        ClipboardBackend::Wayland => Command::new("wl-copy").stdin(Stdio::piped()).spawn(),
        ClipboardBackend::X11 => Command::new("xclip")
            .args(["-selection", "clipboard"])
            .stdin(Stdio::piped())
            .spawn(),
        ClipboardBackend::MacOS => Command::new("pbcopy").stdin(Stdio::piped()).spawn(),
        // Read all of stdin as UTF-8 and set the clipboard, so non-ASCII
        // survives (plain `clip.exe` reinterprets via the console codepage).
        ClipboardBackend::Windows => Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                "[Console]::InputEncoding=[System.Text.Encoding]::UTF8; \
                 Set-Clipboard -Value ([Console]::In.ReadToEnd())",
            ])
            .stdin(Stdio::piped())
            .spawn(),
    }
    .context("Failed to spawn clipboard write command")?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(text.as_bytes())
            .context("Failed to write to clipboard command stdin")?;
        // Drop `stdin` here to close the pipe so the child sees EOF.
    }

    let status = child
        .wait()
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
}
