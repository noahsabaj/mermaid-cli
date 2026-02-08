/// Clipboard access for image and text paste
///
/// Auto-detects X11 or Wayland display server and uses the appropriate
/// system tool (xclip or wl-paste) to read clipboard contents.

use anyhow::{Context, Result};
use std::process::Command;

/// Display server type
#[derive(Debug, Clone, Copy)]
enum DisplayServer {
    Wayland,
    X11,
}

/// Detect the active display server
fn detect_display_server() -> Option<DisplayServer> {
    // Check Wayland first
    if std::env::var("WAYLAND_DISPLAY").is_ok() {
        if Command::new("which")
            .arg("wl-paste")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return Some(DisplayServer::Wayland);
        }
    }

    // Fall back to X11
    if std::env::var("DISPLAY").is_ok() {
        if Command::new("which")
            .arg("xclip")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return Some(DisplayServer::X11);
        }
    }

    None
}

/// Check if the clipboard contains image data
pub fn has_image() -> bool {
    match detect_display_server() {
        Some(DisplayServer::Wayland) => Command::new("wl-paste")
            .arg("--list-types")
            .output()
            .map(|o| {
                let types = String::from_utf8_lossy(&o.stdout);
                types.contains("image/png") || types.contains("image/jpeg")
            })
            .unwrap_or(false),
        Some(DisplayServer::X11) => Command::new("xclip")
            .args(["-selection", "clipboard", "-t", "TARGETS", "-o"])
            .output()
            .map(|o| {
                let types = String::from_utf8_lossy(&o.stdout);
                types.contains("image/png") || types.contains("image/jpeg")
            })
            .unwrap_or(false),
        None => false,
    }
}

/// Read image bytes from the clipboard.
/// Returns (bytes, format) where format is "png" or "jpeg".
pub fn read_image_bytes() -> Result<(Vec<u8>, String)> {
    let server =
        detect_display_server().context("No display server detected (need X11 with xclip or Wayland with wl-paste)")?;

    // Try PNG first, then JPEG
    for (mime, format) in [("image/png", "png"), ("image/jpeg", "jpeg")] {
        let output = match server {
            DisplayServer::Wayland => Command::new("wl-paste").args(["--type", mime]).output(),
            DisplayServer::X11 => Command::new("xclip")
                .args(["-selection", "clipboard", "-t", mime, "-o"])
                .output(),
        };

        if let Ok(output) = output {
            if output.status.success() && !output.stdout.is_empty() {
                return Ok((output.stdout, format.to_string()));
            }
        }
    }

    anyhow::bail!("No image data found in clipboard")
}

/// Read text from the clipboard (fallback when no image is found).
pub fn read_text() -> Result<String> {
    let server =
        detect_display_server().context("No display server detected (need X11 with xclip or Wayland with wl-paste)")?;

    let output = match server {
        DisplayServer::Wayland => Command::new("wl-paste")
            .args(["--type", "text/plain"])
            .output(),
        DisplayServer::X11 => Command::new("xclip")
            .args(["-selection", "clipboard", "-o"])
            .output(),
    };

    let output = output.context("Failed to execute clipboard command")?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        anyhow::bail!("Clipboard does not contain text")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_display_server() {
        // Just verify it doesn't panic — actual result depends on environment
        let _ = detect_display_server();
    }

    #[test]
    fn test_has_image_no_crash() {
        // Should return false gracefully if no display server
        let _ = has_image();
    }
}
