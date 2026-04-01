//! Computer use: screenshot capture, mouse/keyboard control
//!
//! Platform detection follows the same pattern as clipboard.rs:
//! - Linux/X11: scrot (screenshot) + xdotool (mouse/keyboard)
//! - Linux/Wayland: grim (screenshot) + ydotool/wtype (mouse/keyboard)
//! - macOS: screencapture + cliclick (stub)
//! - Windows: PowerShell (stub)

use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use super::types::ActionResult;
use crate::constants::SCREENSHOT_MAX_WIDTH;

/// Display server / platform type for computer use
#[derive(Debug, Clone, Copy)]
enum DisplayBackend {
    X11,
    Wayland,
    #[allow(dead_code)]
    MacOS,
    #[allow(dead_code)]
    Windows,
}

/// Monotonic counter for unique temp file names (avoids collisions)
static SCREENSHOT_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Scale factor from the last screenshot (pixels_original / pixels_sent_to_model)
/// Stored as f64 bits in an AtomicU64 for thread safety.
static SCALE_FACTOR: AtomicU64 = AtomicU64::new(0x3FF0_0000_0000_0000); // f64 bits for 1.0

/// Capture offset: top-left corner of the last screenshot in screen coordinates.
/// When capturing a region/window/monitor, model coordinates are relative to the
/// captured area. These offsets translate back to absolute screen coordinates.
static CAPTURE_OFFSET_X: AtomicU64 = AtomicU64::new(0); // f64 bits for 0.0
static CAPTURE_OFFSET_Y: AtomicU64 = AtomicU64::new(0); // f64 bits for 0.0

fn get_scale_factor() -> f64 {
    f64::from_bits(SCALE_FACTOR.load(Ordering::Relaxed))
}

fn set_scale_factor(factor: f64) {
    SCALE_FACTOR.store(factor.to_bits(), Ordering::Relaxed);
}

fn set_capture_offset(x: i32, y: i32) {
    CAPTURE_OFFSET_X.store((x as f64).to_bits(), Ordering::Relaxed);
    CAPTURE_OFFSET_Y.store((y as f64).to_bits(), Ordering::Relaxed);
}

fn get_capture_offset() -> (i32, i32) {
    let x = f64::from_bits(CAPTURE_OFFSET_X.load(Ordering::Relaxed)) as i32;
    let y = f64::from_bits(CAPTURE_OFFSET_Y.load(Ordering::Relaxed)) as i32;
    (x, y)
}

/// Detect the active display backend
fn detect_backend() -> Option<DisplayBackend> {
    if cfg!(target_os = "macos") {
        return Some(DisplayBackend::MacOS);
    }
    if cfg!(target_os = "windows") {
        return Some(DisplayBackend::Windows);
    }

    // Linux: check Wayland first
    if std::env::var("WAYLAND_DISPLAY").is_ok()
        && has_command("grim")
    {
        return Some(DisplayBackend::Wayland);
    }

    // Linux: fall back to X11
    if std::env::var("DISPLAY").is_ok()
        && has_command("scrot")
    {
        return Some(DisplayBackend::X11);
    }

    None
}

/// Check if a command is available on PATH
fn has_command(name: &str) -> bool {
    Command::new("which")
        .arg(name)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Read PNG width from the IHDR chunk header (no image crate needed)
fn read_png_width(bytes: &[u8]) -> Option<u32> {
    if bytes.len() > 24 && &bytes[0..8] == b"\x89PNG\r\n\x1a\n" {
        Some(u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]))
    } else {
        None
    }
}

/// Read PNG height from the IHDR chunk header
fn read_png_height(bytes: &[u8]) -> Option<u32> {
    if bytes.len() > 28 && &bytes[0..8] == b"\x89PNG\r\n\x1a\n" {
        Some(u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]))
    } else {
        None
    }
}

/// Downscale a PNG file if wider than max_width. Returns the scale factor.
/// Falls back to 1.0 (no scaling) if ImageMagick is not available.
fn downscale_if_needed(path: &str, max_width: u32) -> Result<f64> {
    let bytes = std::fs::read(path)?;
    let original_width = read_png_width(&bytes).unwrap_or(1920);

    if original_width <= max_width {
        return Ok(1.0);
    }

    let scale_factor = original_width as f64 / max_width as f64;

    // Try ImageMagick convert (most common)
    let output_path = format!("{}.scaled.png", path);
    let result = Command::new("convert")
        .args([path, "-resize", &format!("{}x", max_width), &output_path])
        .output();

    if let Ok(output) = result
        && output.status.success()
    {
        // Replace original with scaled version
        std::fs::rename(&output_path, path)?;
        return Ok(scale_factor);
    }

    // Try ffmpeg as fallback
    let result = Command::new("ffmpeg")
        .args([
            "-y", "-i", path, "-vf",
            &format!("scale={}:-1", max_width),
            &output_path,
        ])
        .output();

    if let Ok(output) = result
        && output.status.success()
    {
        std::fs::rename(&output_path, path)?;
        return Ok(scale_factor);
    }

    // No downscaler available -- send full resolution
    let _ = std::fs::remove_file(&output_path);
    tracing::warn!(
        "Neither ImageMagick nor ffmpeg available for screenshot downscaling. Sending full {}px width.",
        original_width
    );
    Ok(1.0)
}

/// Scale model coordinates back to actual screen coordinates.
/// Applies both the downscale factor and the capture region offset.
fn scale_coords(x: i32, y: i32) -> (i32, i32) {
    let factor = get_scale_factor();
    let (ox, oy) = get_capture_offset();
    (
        (x as f64 * factor) as i32 + ox,
        (y as f64 * factor) as i32 + oy,
    )
}

// ===== Geometry Helpers =====

/// Parse monitor geometry from xrandr output.
/// Returns (x_offset, y_offset, width, height) for the named output.
fn parse_monitor_geometry_x11(name: &str) -> Option<(i32, i32, u32, u32)> {
    let output = Command::new("xrandr").arg("--query").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Look for: "DP-0 connected 5120x2880+0+0" or "DP-0 connected primary 5120x2880+5120+0"
    for line in stdout.lines() {
        if !line.contains(" connected") {
            continue;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.first() != Some(&name) {
            continue;
        }
        // Find the WxH+X+Y token (skip "connected", optional "primary")
        for part in &parts[2..] {
            if let Some((res, offsets)) = part.split_once('+')
                && let Some((w, h)) = res.split_once('x')
            {
                let width = w.parse::<u32>().ok()?;
                let height = h.parse::<u32>().ok()?;
                let mut offset_parts = offsets.splitn(2, '+');
                let x = offset_parts.next()?.parse::<i32>().ok()?;
                let y = offset_parts.next()?.parse::<i32>().ok()?;
                return Some((x, y, width, height));
            }
        }
    }
    None
}

/// List available monitor names from xrandr (for error messages).
fn list_monitors_x11() -> Vec<String> {
    let output = Command::new("xrandr").arg("--query").output();
    let Ok(output) = output else { return Vec::new() };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| l.contains(" connected"))
        .filter_map(|l| l.split_whitespace().next().map(|s| s.to_string()))
        .collect()
}

/// Get geometry of a window by its WID via xdotool.
/// Returns (x_offset, y_offset, width, height).
fn get_window_geometry_x11(wid: &str) -> Option<(i32, i32, u32, u32)> {
    let geom_output = Command::new("xdotool")
        .args(["getwindowgeometry", "--shell", wid])
        .output()
        .ok()?;
    if !geom_output.status.success() {
        return None;
    }

    let geom = String::from_utf8_lossy(&geom_output.stdout);
    let mut x = None;
    let mut y = None;
    let mut width = None;
    let mut height = None;
    for line in geom.lines() {
        if let Some(val) = line.strip_prefix("X=") {
            x = val.parse().ok();
        } else if let Some(val) = line.strip_prefix("Y=") {
            y = val.parse().ok();
        } else if let Some(val) = line.strip_prefix("WIDTH=") {
            width = val.parse().ok();
        } else if let Some(val) = line.strip_prefix("HEIGHT=") {
            height = val.parse().ok();
        }
    }
    Some((x?, y?, width?, height?))
}

/// Get focused window geometry via xdotool (convenience wrapper).
fn get_focused_window_geometry_x11() -> Option<(i32, i32, u32, u32)> {
    let wid_output = Command::new("xdotool").arg("getactivewindow").output().ok()?;
    if !wid_output.status.success() {
        return None;
    }
    let wid = String::from_utf8_lossy(&wid_output.stdout).trim().to_string();
    get_window_geometry_x11(&wid)
}

/// Parse a region string "X,Y,WIDTHxHEIGHT" into (x, y, width, height).
fn parse_region_string(region: &str) -> Option<(i32, i32, u32, u32)> {
    // Format: "X,Y,WIDTHxHEIGHT" e.g., "0,0,1920x1080"
    let parts: Vec<&str> = region.splitn(3, ',').collect();
    if parts.len() != 3 {
        return None;
    }
    let x = parts[0].parse::<i32>().ok()?;
    let y = parts[1].parse::<i32>().ok()?;
    let (w, h) = parts[2].split_once('x')?;
    let width = w.parse::<u32>().ok()?;
    let height = h.parse::<u32>().ok()?;
    Some((x, y, width, height))
}

/// Capture the currently focused window, downscale, encode to base64.
/// Sets SCALE_FACTOR and CAPTURE_OFFSET as side effects.
/// Returns (description, base64_png) on success.
/// Used by auto-screenshot after click/type/key.
fn capture_focused_window_image(backend: DisplayBackend) -> Result<(String, String)> {
    let seq = SCREENSHOT_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp_path = std::env::temp_dir().join(format!("mermaid-auto-screenshot-{}.png", seq));
    let temp_str = temp_path.to_string_lossy().to_string();

    let mut offset_x: i32 = 0;
    let mut offset_y: i32 = 0;

    match backend {
        DisplayBackend::X11 => {
            if let Some((wx, wy, _, _)) = get_focused_window_geometry_x11() {
                offset_x = wx;
                offset_y = wy;
            }
            let output = Command::new("scrot")
                .args(["-u", "-o", &temp_str])
                .output()
                .context("Failed to run scrot -u for auto-screenshot")?;
            if !output.status.success() {
                anyhow::bail!(
                    "scrot -u failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        },
        DisplayBackend::Wayland => {
            let output = Command::new("grim")
                .arg(&temp_str)
                .output()
                .context("Failed to run grim for auto-screenshot")?;
            if !output.status.success() {
                anyhow::bail!(
                    "grim failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        },
        _ => anyhow::bail!("Unsupported platform for auto-screenshot"),
    }

    let scale_factor = downscale_if_needed(&temp_str, SCREENSHOT_MAX_WIDTH)?;
    set_scale_factor(scale_factor);
    set_capture_offset(offset_x, offset_y);

    let bytes = std::fs::read(&temp_path)?;
    let width = read_png_width(&bytes).unwrap_or(0);
    let height = read_png_height(&bytes).unwrap_or(0);
    let base64_png = general_purpose::STANDARD.encode(&bytes);
    let _ = std::fs::remove_file(&temp_path);

    let offset_info = if offset_x != 0 || offset_y != 0 {
        format!(", offset: +{}+{}", offset_x, offset_y)
    } else {
        String::new()
    };

    Ok((
        format!(
            "focused window {}x{}, scale: {:.2}x{}",
            width, height, scale_factor, offset_info
        ),
        base64_png,
    ))
}

// ===== Public API =====

/// Capture a screenshot and return it as base64 PNG.
///
/// Modes:
/// - "fullscreen" (default): entire screen
/// - "focused": active/focused window only
/// - "monitor": single monitor by name (e.g., "DP-0")
/// - "region": rectangular area by pixel coordinates ("X,Y,WIDTHxHEIGHT")
pub async fn execute_screenshot(
    mode: &str,
    monitor: Option<&str>,
    region: Option<&str>,
    window: Option<&str>,
) -> ActionResult {
    let backend = match detect_backend() {
        Some(b) => b,
        None => {
            return ActionResult::Error {
                error: "No display backend detected. Need scrot (X11) or grim (Wayland)."
                    .to_string(),
            };
        },
    };

    let seq = SCREENSHOT_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp_path = std::env::temp_dir().join(format!("mermaid-screenshot-{}.png", seq));
    let temp_str = temp_path.to_string_lossy().to_string();

    // Determine capture offset (for coordinate translation after clicks)
    let mut offset_x: i32 = 0;
    let mut offset_y: i32 = 0;

    // Capture screenshot based on mode
    let capture_result = match mode {
        "focused" => match backend {
            DisplayBackend::X11 => {
                // Get focused window geometry for offset tracking
                if let Some((wx, wy, _, _)) = get_focused_window_geometry_x11() {
                    offset_x = wx;
                    offset_y = wy;
                }
                Command::new("scrot")
                    .args(["-u", "-o", &temp_str])
                    .output()
                    .context("Failed to run scrot -u (focused window)")
            },
            DisplayBackend::Wayland => {
                // grim doesn't support focused window directly; fall back to fullscreen
                Command::new("grim")
                    .arg(&temp_str)
                    .output()
                    .context("Failed to run grim (Wayland focused fallback)")
            },
            _ => return unsupported_platform_error(),
        },
        "monitor" => {
            let monitor_name = match monitor {
                Some(name) => name,
                None => {
                    let available = list_monitors_x11();
                    return ActionResult::Error {
                        error: format!(
                            "Monitor name required for 'monitor' mode. Available: {}",
                            if available.is_empty() {
                                "none detected".to_string()
                            } else {
                                available.join(", ")
                            }
                        ),
                    };
                },
            };
            match backend {
                DisplayBackend::X11 => {
                    // Parse xrandr to get monitor bounds, use scrot -a
                    if let Some((mx, my, mw, mh)) = parse_monitor_geometry_x11(monitor_name) {
                        offset_x = mx;
                        offset_y = my;
                        Command::new("scrot")
                            .args([
                                "-a",
                                &format!("{},{},{},{}", mx, my, mw, mh),
                                "-o",
                                &temp_str,
                            ])
                            .output()
                            .context("Failed to run scrot -a (monitor region)")
                    } else {
                        let available = list_monitors_x11();
                        return ActionResult::Error {
                            error: format!(
                                "Monitor '{}' not found. Available: {}",
                                monitor_name,
                                available.join(", ")
                            ),
                        };
                    }
                },
                DisplayBackend::Wayland => {
                    // grim -o OUTPUT_NAME
                    Command::new("grim")
                        .args(["-o", monitor_name, &temp_str])
                        .output()
                        .context("Failed to run grim -o (monitor)")
                },
                _ => return unsupported_platform_error(),
            }
        },
        "region" => {
            let region_str = match region {
                Some(r) => r,
                None => {
                    return ActionResult::Error {
                        error: "Region required for 'region' mode. Format: 'X,Y,WIDTHxHEIGHT'"
                            .to_string(),
                    };
                },
            };
            let (rx, ry, rw, rh) = match parse_region_string(region_str) {
                Some(r) => r,
                None => {
                    return ActionResult::Error {
                        error: format!(
                            "Invalid region format '{}'. Expected 'X,Y,WIDTHxHEIGHT' (e.g., '0,0,1920x1080')",
                            region_str
                        ),
                    };
                },
            };
            offset_x = rx;
            offset_y = ry;
            match backend {
                DisplayBackend::X11 => Command::new("scrot")
                    .args([
                        "-a",
                        &format!("{},{},{},{}", rx, ry, rw, rh),
                        "-o",
                        &temp_str,
                    ])
                    .output()
                    .context("Failed to run scrot -a (region)"),
                DisplayBackend::Wayland => Command::new("grim")
                    .args(["-g", &format!("{},{} {}x{}", rx, ry, rw, rh), &temp_str])
                    .output()
                    .context("Failed to run grim -g (region)"),
                _ => return unsupported_platform_error(),
            }
        },
        "window" => {
            let window_name = match window {
                Some(name) => name,
                None => {
                    return ActionResult::Error {
                        error: "Window name required for 'window' mode. Use list_windows to see available windows.".to_string(),
                    };
                },
            };
            match backend {
                DisplayBackend::X11 => {
                    // Search for window by name
                    let search_output = Command::new("xdotool")
                        .args(["search", "--name", window_name])
                        .output();
                    match search_output {
                        Ok(out) if out.status.success() => {
                            let stdout = String::from_utf8_lossy(&out.stdout);
                            let wid = match stdout.lines().next() {
                                Some(id) if !id.trim().is_empty() => id.trim().to_string(),
                                _ => {
                                    return ActionResult::Error {
                                        error: format!(
                                            "No window found matching '{}'. Use list_windows to see available windows.",
                                            window_name
                                        ),
                                    };
                                },
                            };

                            // Activate the window and wait for focus
                            let _ = Command::new("xdotool")
                                .args(["windowactivate", "--sync", &wid])
                                .output();
                            tokio::time::sleep(std::time::Duration::from_millis(200)).await;

                            // Get geometry for offset tracking
                            if let Some((wx, wy, _, _)) = get_window_geometry_x11(&wid) {
                                offset_x = wx;
                                offset_y = wy;
                            }

                            Command::new("scrot")
                                .args(["-u", "-o", &temp_str])
                                .output()
                                .context("Failed to run scrot -u (window capture)")
                        },
                        Ok(out) => {
                            return ActionResult::Error {
                                error: format!(
                                    "Window search failed for '{}': {}",
                                    window_name,
                                    String::from_utf8_lossy(&out.stderr)
                                ),
                            };
                        },
                        Err(e) => {
                            return ActionResult::Error {
                                error: format!("Failed to search for window: {}", e),
                            };
                        },
                    }
                },
                DisplayBackend::Wayland => {
                    return ActionResult::Error {
                        error: "Window-by-name capture not supported on Wayland. Use mode: 'focused' instead.".to_string(),
                    };
                },
                _ => return unsupported_platform_error(),
            }
        },
        _ => {
            // "fullscreen" or any unrecognized value
            match backend {
                DisplayBackend::X11 => Command::new("scrot")
                    .args(["-o", &temp_str])
                    .output()
                    .context("Failed to run scrot"),
                DisplayBackend::Wayland => Command::new("grim")
                    .arg(&temp_str)
                    .output()
                    .context("Failed to run grim"),
                _ => return unsupported_platform_error(),
            }
        },
    };

    match capture_result {
        Ok(output) if output.status.success() => {},
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return ActionResult::Error {
                error: format!("Screenshot capture failed: {}", stderr),
            };
        },
        Err(e) => {
            return ActionResult::Error {
                error: format!("Screenshot capture error: {}", e),
            };
        },
    }

    // Downscale if needed
    let scale_factor = match downscale_if_needed(&temp_str, SCREENSHOT_MAX_WIDTH) {
        Ok(f) => f,
        Err(e) => {
            let _ = std::fs::remove_file(&temp_path);
            return ActionResult::Error {
                error: format!("Screenshot processing error: {}", e),
            };
        },
    };
    set_scale_factor(scale_factor);
    set_capture_offset(offset_x, offset_y);

    // Read and encode
    let bytes = match std::fs::read(&temp_path) {
        Ok(b) => b,
        Err(e) => {
            let _ = std::fs::remove_file(&temp_path);
            return ActionResult::Error {
                error: format!("Failed to read screenshot: {}", e),
            };
        },
    };

    let width = read_png_width(&bytes).unwrap_or(0);
    let height = read_png_height(&bytes).unwrap_or(0);
    let base64_png = general_purpose::STANDARD.encode(&bytes);
    let _ = std::fs::remove_file(&temp_path);

    // Build informative output message
    let mode_info = match mode {
        "focused" => "focused window".to_string(),
        "monitor" => format!("monitor {}", monitor.unwrap_or("?")),
        "region" => format!("region {}", region.unwrap_or("?")),
        "window" => format!("window \"{}\"", window.unwrap_or("?")),
        _ => "fullscreen".to_string(),
    };
    let offset_info = if offset_x != 0 || offset_y != 0 {
        format!(", offset: +{}+{}", offset_x, offset_y)
    } else {
        String::new()
    };

    ActionResult::Success {
        output: format!(
            "Screenshot captured ({}, {}x{}, scale: {:.2}x{})",
            mode_info, width, height, scale_factor, offset_info
        ),
        images: Some(vec![base64_png]),
    }
}

/// List all visible window titles (X11 only)
pub async fn execute_list_windows() -> ActionResult {
    let backend = match detect_backend() {
        Some(b) => b,
        None => return no_backend_error(),
    };

    match backend {
        DisplayBackend::X11 => {
            if !has_command("xdotool") {
                return ActionResult::Error {
                    error: "xdotool required for listing windows".to_string(),
                };
            }

            let output = Command::new("xdotool")
                .args(["search", "--onlyvisible", "--name", ""])
                .output();

            match output {
                Ok(out) if out.status.success() => {
                    let wids = String::from_utf8_lossy(&out.stdout);
                    let mut windows = Vec::new();

                    for wid in wids.lines() {
                        let wid = wid.trim();
                        if wid.is_empty() {
                            continue;
                        }
                        if let Ok(name_out) = Command::new("xdotool")
                            .args(["getwindowname", wid])
                            .output()
                            && name_out.status.success()
                        {
                            let name = String::from_utf8_lossy(&name_out.stdout)
                                .trim()
                                .to_string();
                            if !name.is_empty() && !windows.contains(&name) {
                                windows.push(name);
                            }
                        }
                    }

                    if windows.is_empty() {
                        ActionResult::Success {
                            output: "No visible windows found.".to_string(),
                            images: None,
                        }
                    } else {
                        let list = windows
                            .iter()
                            .map(|w| format!("  - {}", w))
                            .collect::<Vec<_>>()
                            .join("\n");
                        ActionResult::Success {
                            output: format!(
                                "Visible windows ({}):\n{}",
                                windows.len(),
                                list
                            ),
                            images: None,
                        }
                    }
                },
                Ok(out) => ActionResult::Error {
                    error: format!(
                        "xdotool search failed: {}",
                        String::from_utf8_lossy(&out.stderr)
                    ),
                },
                Err(e) => ActionResult::Error {
                    error: format!("Failed to list windows: {}", e),
                },
            }
        },
        DisplayBackend::Wayland => ActionResult::Error {
            error: "list_windows not supported on Wayland. Window management requires X11 + xdotool."
                .to_string(),
        },
        _ => unsupported_platform_error(),
    }
}

/// Click at screen coordinates
pub async fn execute_click(x: i32, y: i32, button: &str) -> ActionResult {
    let backend = match detect_backend() {
        Some(b) => b,
        None => return no_backend_error(),
    };

    let (sx, sy) = scale_coords(x, y);
    let button_code = match button {
        "left" => "1",
        "middle" => "2",
        "right" => "3",
        _ => "1",
    };

    // Atomic move+click with --sync to wait for the window manager to process
    let result = match backend {
        DisplayBackend::X11 => Command::new("xdotool")
            .args([
                "mousemove", "--sync", &sx.to_string(), &sy.to_string(),
                "click", "--clearmodifiers", button_code,
            ])
            .output(),
        DisplayBackend::Wayland => {
            if !has_command("ydotool") {
                return ActionResult::Error {
                    error: "ydotool required for Wayland mouse control".to_string(),
                };
            }
            Command::new("ydotool")
                .args([
                    "mousemove", "--absolute",
                    "-x", &sx.to_string(),
                    "-y", &sy.to_string(),
                ])
                .output()
                .and_then(|_| {
                    Command::new("ydotool")
                        .args(["click", &format!("0x{}", button_code)])
                        .output()
                })
        },
        _ => return unsupported_platform_error(),
    };

    // Pause after click to let window manager process focus change + UI update
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    match result {
        Ok(output) if output.status.success() => {
            let click_msg =
                format!("Clicked {} at ({}, {}) [screen: ({}, {})]", button, x, y, sx, sy);

            // Auto-screenshot: capture focused window so model sees the result
            match capture_focused_window_image(backend) {
                Ok((img_desc, base64_png)) => ActionResult::Success {
                    output: format!("{}\n[auto-screenshot: {}]", click_msg, img_desc),
                    images: Some(vec![base64_png]),
                },
                Err(_) => ActionResult::Success {
                    output: click_msg,
                    images: None,
                },
            }
        },
        Ok(output) => ActionResult::Error {
            error: format!("Click failed: {}", String::from_utf8_lossy(&output.stderr)),
        },
        Err(e) => ActionResult::Error {
            error: format!("Click error: {}", e),
        },
    }
}

/// Type text at the current cursor position
pub async fn execute_type_text(text: &str) -> ActionResult {
    let backend = match detect_backend() {
        Some(b) => b,
        None => return no_backend_error(),
    };

    let result = match backend {
        DisplayBackend::X11 => Command::new("xdotool")
            .args(["type", "--clearmodifiers", "--delay", "12", text])
            .output(),
        DisplayBackend::Wayland => {
            if has_command("wtype") {
                Command::new("wtype").arg(text).output()
            } else if has_command("ydotool") {
                Command::new("ydotool")
                    .args(["type", "--delay", "12", text])
                    .output()
            } else {
                return ActionResult::Error {
                    error: "wtype or ydotool required for Wayland text input".to_string(),
                };
            }
        },
        _ => return unsupported_platform_error(),
    };

    match result {
        Ok(output) if output.status.success() => {
            let type_msg = format!("Typed: {}", text.chars().take(50).collect::<String>());

            // Auto-screenshot after typing
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            match capture_focused_window_image(backend) {
                Ok((img_desc, base64_png)) => ActionResult::Success {
                    output: format!("{}\n[auto-screenshot: {}]", type_msg, img_desc),
                    images: Some(vec![base64_png]),
                },
                Err(_) => ActionResult::Success {
                    output: type_msg,
                    images: None,
                },
            }
        },
        Ok(output) => ActionResult::Error {
            error: format!("Type failed: {}", String::from_utf8_lossy(&output.stderr)),
        },
        Err(e) => ActionResult::Error {
            error: format!("Type error: {}", e),
        },
    }
}

/// Press a key or key combination
pub async fn execute_press_key(key: &str) -> ActionResult {
    let backend = match detect_backend() {
        Some(b) => b,
        None => return no_backend_error(),
    };

    let result = match backend {
        DisplayBackend::X11 => Command::new("xdotool")
            .args(["key", key])
            .output(),
        DisplayBackend::Wayland => {
            if has_command("wtype") {
                // wtype uses -k for key names, -M/-m for modifiers
                let parts: Vec<&str> = key.split('+').collect();
                let mut args = Vec::new();
                for (i, part) in parts.iter().enumerate() {
                    if i < parts.len() - 1 {
                        // Modifier
                        args.push("-M".to_string());
                        args.push(part.to_string());
                    } else {
                        // Final key
                        args.push("-k".to_string());
                        args.push(part.to_string());
                    }
                }
                // Release modifiers
                for part in parts.iter().take(parts.len().saturating_sub(1)) {
                    args.push("-m".to_string());
                    args.push(part.to_string());
                }
                Command::new("wtype")
                    .args(&args)
                    .output()
            } else if has_command("ydotool") {
                Command::new("ydotool")
                    .args(["key", key])
                    .output()
            } else {
                return ActionResult::Error {
                    error: "wtype or ydotool required for Wayland key input".to_string(),
                };
            }
        },
        _ => return unsupported_platform_error(),
    };

    match result {
        Ok(output) if output.status.success() => {
            let key_msg = format!("Pressed: {}", key);

            // Auto-screenshot after key press
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            match capture_focused_window_image(backend) {
                Ok((img_desc, base64_png)) => ActionResult::Success {
                    output: format!("{}\n[auto-screenshot: {}]", key_msg, img_desc),
                    images: Some(vec![base64_png]),
                },
                Err(_) => ActionResult::Success {
                    output: key_msg,
                    images: None,
                },
            }
        },
        Ok(output) => ActionResult::Error {
            error: format!("Key press failed: {}", String::from_utf8_lossy(&output.stderr)),
        },
        Err(e) => ActionResult::Error {
            error: format!("Key press error: {}", e),
        },
    }
}

/// Scroll in a direction
pub async fn execute_scroll(direction: &str, amount: i32) -> ActionResult {
    let backend = match detect_backend() {
        Some(b) => b,
        None => return no_backend_error(),
    };

    let result = match backend {
        DisplayBackend::X11 => {
            // xdotool: button 4 = scroll up, button 5 = scroll down
            let button = if direction == "up" { "4" } else { "5" };
            let mut args = Vec::new();
            for _ in 0..amount.max(1) {
                args.push("click");
                args.push(button);
            }
            Command::new("xdotool").args(&args).output()
        },
        DisplayBackend::Wayland => {
            if !has_command("ydotool") {
                return ActionResult::Error {
                    error: "ydotool required for Wayland scroll".to_string(),
                };
            }
            let wheel_amount = if direction == "up" { -amount } else { amount };
            Command::new("ydotool")
                .args(["mousemove", "--wheel", &wheel_amount.to_string()])
                .output()
        },
        _ => return unsupported_platform_error(),
    };

    match result {
        Ok(output) if output.status.success() => ActionResult::Success {
            output: format!("Scrolled {} by {}", direction, amount),
            images: None,
        },
        Ok(output) => ActionResult::Error {
            error: format!("Scroll failed: {}", String::from_utf8_lossy(&output.stderr)),
        },
        Err(e) => ActionResult::Error {
            error: format!("Scroll error: {}", e),
        },
    }
}

/// Move mouse cursor to coordinates
pub async fn execute_mouse_move(x: i32, y: i32) -> ActionResult {
    let backend = match detect_backend() {
        Some(b) => b,
        None => return no_backend_error(),
    };

    let (sx, sy) = scale_coords(x, y);

    let result = match backend {
        DisplayBackend::X11 => Command::new("xdotool")
            .args(["mousemove", "--sync", &sx.to_string(), &sy.to_string()])
            .output(),
        DisplayBackend::Wayland => {
            if !has_command("ydotool") {
                return ActionResult::Error {
                    error: "ydotool required for Wayland mouse control".to_string(),
                };
            }
            Command::new("ydotool")
                .args([
                    "mousemove", "--absolute",
                    "-x", &sx.to_string(),
                    "-y", &sy.to_string(),
                ])
                .output()
        },
        _ => return unsupported_platform_error(),
    };

    match result {
        Ok(output) if output.status.success() => ActionResult::Success {
            output: format!("Moved to ({}, {}) [screen: ({}, {})]", x, y, sx, sy),
            images: None,
        },
        Ok(output) => ActionResult::Error {
            error: format!("Mouse move failed: {}", String::from_utf8_lossy(&output.stderr)),
        },
        Err(e) => ActionResult::Error {
            error: format!("Mouse move error: {}", e),
        },
    }
}

fn no_backend_error() -> ActionResult {
    ActionResult::Error {
        error: "No display backend detected. Install scrot+xdotool (X11) or grim+ydotool (Wayland).".to_string(),
    }
}

fn unsupported_platform_error() -> ActionResult {
    ActionResult::Error {
        error: "Computer use not yet implemented for this platform".to_string(),
    }
}
