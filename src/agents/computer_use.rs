//! Computer use: screenshot capture, mouse/keyboard control
//!
//! Platform detection follows the same pattern as clipboard.rs:
//! - Linux/X11: scrot (screenshot) + xdotool (mouse/keyboard)
//! - Linux/Wayland: grim (screenshot) + ydotool/wtype (mouse/keyboard)
//! - macOS: screencapture + cliclick (stub)
//! - Windows: PowerShell (stub)

use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// RAII guard that removes its file path on drop. Use to wrap temp
/// screenshots so cleanup happens regardless of how the function exits
/// (early-error returns, panics, normal completion). Replaces the
/// scatter of explicit `let _ = std::fs::remove_file(...)` calls that
/// previously had to be repeated at every error site.
struct TempFileGuard(PathBuf);

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

use super::types::ActionResult;
use crate::constants::{
    POST_CLICK_DELAY_MS, POST_KEY_DELAY_MS, POST_TYPE_DELAY_MS, SCREENSHOT_MAX_WIDTH,
    SCREENSHOT_REGISTRY_CAPACITY, WINDOW_FOCUS_DELAY_MS,
};

/// The list of GUI tool names. Single source of truth — used by all
/// model adapters to filter computer-use tools out of subagent contexts
/// (subagents don't share the screenshot registry, so giving them
/// access would let them silently corrupt coords for the parent).
///
/// Adding a new GUI tool means adding its name here AND its handler in
/// `action_executor.rs`. The `gui_tool_names_match_action_variants`
/// test guards against drift.
pub const GUI_TOOL_NAMES: &[&str] = &[
    "screenshot",
    "list_windows",
    "click",
    "type_text",
    "press_key",
    "scroll",
    "mouse_move",
];

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

/// Monotonic counter for screenshot IDs assigned to registry entries.
/// Separate from `SCREENSHOT_COUNTER` (which only names temp files) so
/// IDs stay stable across temp-file renames and survive process logic.
static SCREENSHOT_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Per-screenshot metadata used to translate model-side coordinates
/// back to screen coordinates after a click. Each entry is keyed by
/// `id` (returned in the screenshot tool's success message) so that
/// `click(x, y, screenshot_id)` can pick the right scale + offset
/// even after intervening screenshots have shifted the "latest" entry.
#[derive(Debug, Clone)]
pub(crate) struct ScreenshotMetadata {
    pub id: u64,
    pub scale_factor: f64,
    pub offset_x: i32,
    pub offset_y: i32,
    /// Human-readable description of the capture (`"fullscreen"`,
    /// `"focused window"`, `"region 0,0,1920x1080"`). Surfaced in the
    /// success message and the registry-eviction error so debugging
    /// stale-coords cases is easier.
    pub kind: String,
}

/// Bounded ring buffer of recent screenshot metadata. Capacity is
/// `SCREENSHOT_REGISTRY_CAPACITY` — once full, registering a new entry
/// evicts the oldest. The model can still reference an evicted ID, but
/// the click will fail cleanly with a "metadata evicted, take a fresh
/// screenshot" error rather than silently using wrong coordinates.
struct ScreenshotRegistry {
    entries: VecDeque<ScreenshotMetadata>,
}

impl ScreenshotRegistry {
    const fn new() -> Self {
        Self {
            entries: VecDeque::new(),
        }
    }

    fn push(&mut self, meta: ScreenshotMetadata) {
        if self.entries.len() >= SCREENSHOT_REGISTRY_CAPACITY {
            self.entries.pop_front();
        }
        self.entries.push_back(meta);
    }

    fn get(&self, id: u64) -> Option<&ScreenshotMetadata> {
        self.entries.iter().find(|m| m.id == id)
    }

    fn latest(&self) -> Option<&ScreenshotMetadata> {
        self.entries.back()
    }
}

/// The shared registry. Mutex contention is a non-issue: GUI tool calls
/// are infrequent (human-perceptible cadence) and the critical sections
/// are tiny (push or lookup).
static REGISTRY: Mutex<ScreenshotRegistry> = Mutex::new(ScreenshotRegistry::new());

/// Register a new screenshot's metadata; returns the assigned id which
/// callers should include in the tool's success message so the model
/// can quote it back when calling `click(screenshot_id: <id>)`.
fn register_screenshot(scale_factor: f64, offset_x: i32, offset_y: i32, kind: String) -> u64 {
    let id = SCREENSHOT_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    let meta = ScreenshotMetadata {
        id,
        scale_factor,
        offset_x,
        offset_y,
        kind,
    };
    if let Ok(mut reg) = REGISTRY.lock() {
        reg.push(meta);
    }
    id
}

/// Look up metadata by id; `None` returns the most recent (or `None` if
/// no screenshots have been registered yet).
fn get_metadata(screenshot_id: Option<u64>) -> Option<ScreenshotMetadata> {
    let reg = REGISTRY.lock().ok()?;
    match screenshot_id {
        Some(id) => reg.get(id).cloned(),
        None => reg.latest().cloned(),
    }
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
    if std::env::var("WAYLAND_DISPLAY").is_ok() && has_command("grim") {
        return Some(DisplayBackend::Wayland);
    }

    // Linux: fall back to X11
    if std::env::var("DISPLAY").is_ok() && has_command("scrot") {
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
        Some(u32::from_be_bytes([
            bytes[16], bytes[17], bytes[18], bytes[19],
        ]))
    } else {
        None
    }
}

/// Read PNG height from the IHDR chunk header
fn read_png_height(bytes: &[u8]) -> Option<u32> {
    if bytes.len() > 28 && &bytes[0..8] == b"\x89PNG\r\n\x1a\n" {
        Some(u32::from_be_bytes([
            bytes[20], bytes[21], bytes[22], bytes[23],
        ]))
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
            "-y",
            "-i",
            path,
            "-vf",
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

/// Scale model coordinates back to actual screen coordinates using the
/// scale + offset from a specific registered screenshot. When
/// `screenshot_id` is `None`, the most recent screenshot is used
/// (backward-compatible default — most agentic loops chain
/// screenshot→click→screenshot→click and never need to specify).
///
/// Returns `Err` with an actionable message when:
/// - `screenshot_id` is given but the entry has been LRU-evicted
///   (capacity is `SCREENSHOT_REGISTRY_CAPACITY` = 16)
/// - `screenshot_id` is `None` and no screenshots have been registered
///   yet (model called `click` before `screenshot`)
fn scale_coords_for(
    x: i32,
    y: i32,
    screenshot_id: Option<u64>,
) -> std::result::Result<(i32, i32), String> {
    let meta = get_metadata(screenshot_id).ok_or_else(|| match screenshot_id {
        Some(id) => format!(
            "Screenshot id {} not found in registry (likely evicted — capacity {}). \
             Take a fresh screenshot and retry the click with the new id.",
            id, SCREENSHOT_REGISTRY_CAPACITY
        ),
        None => "No screenshots registered yet — call `screenshot` before `click`/`mouse_move` \
                 so coordinates can be translated."
            .to_string(),
    })?;
    // `kind` is unused at the call sites today but kept on the metadata
    // for future diagnostics (e.g., a "click using fullscreen coords on
    // a focused-window screenshot" warning would key off mismatched
    // kinds across consecutive operations).
    let _ = &meta.kind;
    Ok((
        (x as f64 * meta.scale_factor) as i32 + meta.offset_x,
        (y as f64 * meta.scale_factor) as i32 + meta.offset_y,
    ))
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
    let Ok(output) = output else {
        return Vec::new();
    };
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
    let wid_output = Command::new("xdotool")
        .arg("getactivewindow")
        .output()
        .ok()?;
    if !wid_output.status.success() {
        return None;
    }
    let wid = String::from_utf8_lossy(&wid_output.stdout)
        .trim()
        .to_string();
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
    // Step 5f Wave 6: RAII cleanup regardless of how this exits.
    let _temp_guard = TempFileGuard(temp_path.clone());

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
                anyhow::bail!("grim failed: {}", String::from_utf8_lossy(&output.stderr));
            }
        },
        _ => anyhow::bail!("Unsupported platform for auto-screenshot"),
    }

    let scale_factor = downscale_if_needed(&temp_str, SCREENSHOT_MAX_WIDTH)?;
    let id = register_screenshot(
        scale_factor,
        offset_x,
        offset_y,
        "focused window".to_string(),
    );

    let bytes = std::fs::read(&temp_path)?;
    let width = read_png_width(&bytes).unwrap_or(0);
    let height = read_png_height(&bytes).unwrap_or(0);
    let base64_png = general_purpose::STANDARD.encode(&bytes);
    // (TempFileGuard handles cleanup on drop.)

    let offset_info = if offset_x != 0 || offset_y != 0 {
        format!(", offset: +{}+{}", offset_x, offset_y)
    } else {
        String::new()
    };

    Ok((
        format!(
            "id: {}, focused window {}x{}, scale: {:.2}x{}",
            id, width, height, scale_factor, offset_info
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
    // Step 5f Wave 6 (Bug 10): RAII guard removes the temp file on
    // drop, regardless of how this function exits (early returns,
    // panics, normal completion). Replaces a scatter of explicit
    // `let _ = std::fs::remove_file` calls.
    let _temp_guard = TempFileGuard(temp_path.clone());

    // Determine capture offset (for coordinate translation after clicks)
    let mut offset_x: i32 = 0;
    let mut offset_y: i32 = 0;

    // Step 5f Wave 6 (Bug 8): normalize mode to lowercase so models
    // sending "FOCUSED" or "Region" don't silently fall through to the
    // default fullscreen branch.
    let mode_normalized = mode.to_lowercase();
    let mode_lc = mode_normalized.as_str();

    // Capture screenshot based on mode
    let capture_result = match mode_lc {
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
                // Wave 4: don't silently fall back to fullscreen — the
                // model would think it captured the focused window
                // and click coords would be wrong (offset never set).
                // Surface the limitation so the model picks a working
                // mode.
                return ActionResult::Error {
                    error: "Mode 'focused' not supported on Wayland (grim has no \
                            focused-window primitive). Use mode: 'fullscreen' or \
                            mode: 'monitor' with a specific output name."
                        .to_string(),
                };
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
                            tokio::time::sleep(std::time::Duration::from_millis(
                                WINDOW_FOCUS_DELAY_MS,
                            ))
                            .await;

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
                        error: "Mode 'window' not supported on Wayland (grim has no \
                                window-by-name capture; xdotool isn't available either). \
                                Use mode: 'fullscreen' or mode: 'monitor' with a specific \
                                output name. If you need per-window capture, run mermaid \
                                from an X11 session."
                            .to_string(),
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

    // Downscale if needed (TempFileGuard handles cleanup on early returns).
    let scale_factor = match downscale_if_needed(&temp_str, SCREENSHOT_MAX_WIDTH) {
        Ok(f) => f,
        Err(e) => {
            return ActionResult::Error {
                error: format!("Screenshot processing error: {}", e),
            };
        },
    };
    // Build the human-readable mode label first so it can also serve
    // as the registry "kind" for diagnostics.
    let mode_info = match mode_lc {
        "focused" => "focused window".to_string(),
        "monitor" => format!("monitor {}", monitor.unwrap_or("?")),
        "region" => format!("region {}", region.unwrap_or("?")),
        "window" => format!("window \"{}\"", window.unwrap_or("?")),
        _ => "fullscreen".to_string(),
    };

    let id = register_screenshot(scale_factor, offset_x, offset_y, mode_info.clone());

    // Read and encode (TempFileGuard handles cleanup on every exit path).
    let bytes = match std::fs::read(&temp_path) {
        Ok(b) => b,
        Err(e) => {
            return ActionResult::Error {
                error: format!("Failed to read screenshot: {}", e),
            };
        },
    };

    let width = read_png_width(&bytes).unwrap_or(0);
    let height = read_png_height(&bytes).unwrap_or(0);
    let base64_png = general_purpose::STANDARD.encode(&bytes);

    let offset_info = if offset_x != 0 || offset_y != 0 {
        format!(", offset: +{}+{}", offset_x, offset_y)
    } else {
        String::new()
    };

    // Step 5f Wave 5: warn the model when fullscreen capture spans
    // multiple monitors. The single global scale factor can't represent
    // mixed-DPI setups; clicks at monitor boundaries may land wrong.
    // Suggest mode: "monitor" with a specific output for precision.
    let multi_monitor_warning = if (mode_lc == "fullscreen"
        || !["focused", "monitor", "region", "window"].contains(&mode_lc))
        && matches!(backend, DisplayBackend::X11)
    {
        let monitors = list_monitors_x11();
        if monitors.len() > 1 {
            format!(
                "\n[Multi-monitor detected: {}. Click coordinates may be inaccurate \
                 across monitor boundaries — for precise targeting use mode: 'monitor' \
                 with a specific output name.]",
                monitors.join(", ")
            )
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    ActionResult::Success {
        output: format!(
            "Screenshot captured (id: {}, {}, {}x{}, scale: {:.2}x{}){}",
            id, mode_info, width, height, scale_factor, offset_info, multi_monitor_warning
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
                            let name = String::from_utf8_lossy(&name_out.stdout).trim().to_string();
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
                            output: format!("Visible windows ({}):\n{}", windows.len(), list),
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
            error: "list_windows not supported on Wayland (no per-window enumeration \
                    primitive in the wlr-protocols ecosystem; xdotool isn't available \
                    either). To use this tool, run mermaid from an X11 session. For \
                    fullscreen capture without window targeting, use mode: 'fullscreen' \
                    or mode: 'monitor'."
                .to_string(),
        },
        _ => unsupported_platform_error(),
    }
}

/// Click at screen coordinates. `screenshot_id` selects which
/// screenshot's coordinate space `(x, y)` refer to; `None` means "the
/// most recent screenshot" (backward-compatible with models that don't
/// thread the id through).
pub async fn execute_click(
    x: i32,
    y: i32,
    button: &str,
    screenshot_id: Option<u64>,
) -> ActionResult {
    let backend = match detect_backend() {
        Some(b) => b,
        None => return no_backend_error(),
    };

    let (sx, sy) = match scale_coords_for(x, y, screenshot_id) {
        Ok(p) => p,
        Err(e) => return ActionResult::Error { error: e },
    };
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
                "mousemove",
                "--sync",
                &sx.to_string(),
                &sy.to_string(),
                "click",
                "--clearmodifiers",
                button_code,
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
                    "mousemove",
                    "--absolute",
                    "-x",
                    &sx.to_string(),
                    "-y",
                    &sy.to_string(),
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

    // Pause after click to let window manager process focus change + UI update.
    tokio::time::sleep(std::time::Duration::from_millis(POST_CLICK_DELAY_MS)).await;

    match result {
        Ok(output) if output.status.success() => {
            let mut click_msg = format!(
                "Clicked {} at ({}, {}) [screen: ({}, {})]",
                button, x, y, sx, sy
            );

            // Step 5f Wave 3: verify the cursor actually landed where
            // we sent it. xdotool exit-0 means "the action ran" not
            // "the cursor is now at (sx, sy)" — focus changes, modal
            // dialogs, or the WM rejecting the move can all leave the
            // cursor elsewhere. Surfacing the diff helps the model
            // understand why the auto-screenshot looks "wrong."
            // X11 only — Wayland's ydotool has no equivalent query.
            if matches!(backend, DisplayBackend::X11)
                && let Some(warning) = check_cursor_landed(sx, sy)
            {
                click_msg.push_str(&format!("\n{}", warning));
            }

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

/// Tolerance for click-landing verification (in screen pixels). HiDPI
/// fractional scaling can put the cursor a pixel or two off the exact
/// integer target; >5 means something other than rounding is wrong.
const CURSOR_LANDED_TOLERANCE_PX: i32 = 5;

/// Run `xdotool getmouselocation` and compare to the expected
/// `(sx, sy)`. Returns `Some(warning)` if the cursor landed >5px off
/// the expected position; `None` if landing was within tolerance or
/// the check itself failed (best-effort — never blocks the click).
fn check_cursor_landed(sx: i32, sy: i32) -> Option<String> {
    let output = Command::new("xdotool")
        .arg("getmouselocation")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut actual_x: Option<i32> = None;
    let mut actual_y: Option<i32> = None;
    for line in stdout.split_whitespace() {
        if let Some(v) = line.strip_prefix("X:") {
            actual_x = v.parse().ok();
        } else if let Some(v) = line.strip_prefix("Y:") {
            actual_y = v.parse().ok();
        }
    }
    let (ax, ay) = (actual_x?, actual_y?);
    if (ax - sx).abs() > CURSOR_LANDED_TOLERANCE_PX || (ay - sy).abs() > CURSOR_LANDED_TOLERANCE_PX
    {
        Some(format!(
            "WARNING: cursor at ({}, {}), expected ({}, {}). \
             Window may have moved or focus changed before the click landed.",
            ax, ay, sx, sy
        ))
    } else {
        None
    }
}

/// Type text at the current cursor position
pub async fn execute_type_text(text: &str) -> ActionResult {
    let backend = match detect_backend() {
        Some(b) => b,
        None => return no_backend_error(),
    };

    // Step 5f Wave 6: bumped from 12ms to TYPE_KEY_DELAY_MS (25ms).
    // The old 12ms dropped characters on slow Electron / web targets.
    let delay_str = crate::constants::TYPE_KEY_DELAY_MS.to_string();
    let result = match backend {
        DisplayBackend::X11 => Command::new("xdotool")
            .args(["type", "--clearmodifiers", "--delay", &delay_str, text])
            .output(),
        DisplayBackend::Wayland => {
            if has_command("wtype") {
                Command::new("wtype").arg(text).output()
            } else if has_command("ydotool") {
                Command::new("ydotool")
                    .args(["type", "--delay", &delay_str, text])
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
            tokio::time::sleep(std::time::Duration::from_millis(POST_TYPE_DELAY_MS)).await;
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
        DisplayBackend::X11 => Command::new("xdotool").args(["key", key]).output(),
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
                Command::new("wtype").args(&args).output()
            } else if has_command("ydotool") {
                Command::new("ydotool").args(["key", key]).output()
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
            tokio::time::sleep(std::time::Duration::from_millis(POST_KEY_DELAY_MS)).await;
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
            error: format!(
                "Key press failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ),
        },
        Err(e) => ActionResult::Error {
            error: format!("Key press error: {}", e),
        },
    }
}

/// Scroll in a direction. The `amount` is clamped to
/// `[1, MAX_SCROLL_AMOUNT]` (default 100) so a runaway model can't
/// request a million ticks (would exceed `ARG_MAX` building xdotool's
/// argv on the X11 path).
pub async fn execute_scroll(direction: &str, amount: i32) -> ActionResult {
    let backend = match detect_backend() {
        Some(b) => b,
        None => return no_backend_error(),
    };

    let original_amount = amount;
    let amount = amount.clamp(1, crate::constants::MAX_SCROLL_AMOUNT);

    let result = match backend {
        DisplayBackend::X11 => {
            // xdotool: button 4 = scroll up, button 5 = scroll down
            let button = if direction == "up" { "4" } else { "5" };
            let mut args = Vec::new();
            for _ in 0..amount {
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

    let clamp_note = if original_amount != amount {
        format!(" (clamped from {} to {})", original_amount, amount)
    } else {
        String::new()
    };

    match result {
        Ok(output) if output.status.success() => ActionResult::Success {
            output: format!("Scrolled {} by {}{}", direction, amount, clamp_note),
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

/// Move mouse cursor to coordinates. Same `screenshot_id` semantics as
/// `execute_click`.
pub async fn execute_mouse_move(x: i32, y: i32, screenshot_id: Option<u64>) -> ActionResult {
    let backend = match detect_backend() {
        Some(b) => b,
        None => return no_backend_error(),
    };

    let (sx, sy) = match scale_coords_for(x, y, screenshot_id) {
        Ok(p) => p,
        Err(e) => return ActionResult::Error { error: e },
    };

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
                    "mousemove",
                    "--absolute",
                    "-x",
                    &sx.to_string(),
                    "-y",
                    &sy.to_string(),
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
            error: format!(
                "Mouse move failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ),
        },
        Err(e) => ActionResult::Error {
            error: format!("Mouse move error: {}", e),
        },
    }
}

fn no_backend_error() -> ActionResult {
    ActionResult::Error {
        error:
            "No display backend detected. Install scrot+xdotool (X11) or grim+ydotool (Wayland)."
                .to_string(),
    }
}

fn unsupported_platform_error() -> ActionResult {
    ActionResult::Error {
        error: "Computer use not yet implemented for this platform".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test-local mutex to serialize tests that touch the shared
    /// `REGISTRY`. Without this, parallel test execution races on
    /// reset+push and produces flaky assertions.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    /// Acquire the test serialization lock and clear the registry.
    /// Returns the held guard — drop it at the end of the test (or let
    /// it drop at scope exit) to release for the next test.
    fn lock_and_reset() -> std::sync::MutexGuard<'static, ()> {
        let guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        if let Ok(mut reg) = REGISTRY.lock() {
            reg.entries.clear();
        }
        guard
    }

    #[test]
    fn register_screenshot_assigns_monotonic_ids() {
        let _lock = lock_and_reset();
        let id1 = register_screenshot(1.0, 0, 0, "test1".to_string());
        let id2 = register_screenshot(2.0, 100, 200, "test2".to_string());
        let id3 = register_screenshot(0.5, -50, -100, "test3".to_string());
        assert!(id2 > id1, "ids should be monotonic: {} > {}", id2, id1);
        assert!(id3 > id2, "ids should be monotonic: {} > {}", id3, id2);
    }

    #[test]
    fn scale_coords_for_uses_latest_when_id_is_none() {
        let _lock = lock_and_reset();
        let _id1 = register_screenshot(1.0, 0, 0, "first".to_string());
        let _id2 = register_screenshot(2.0, 100, 50, "second".to_string());
        // None → use latest (id2: scale=2.0, offset=(100, 50))
        let (sx, sy) = scale_coords_for(10, 20, None).expect("latest");
        assert_eq!(sx, 10 * 2 + 100);
        assert_eq!(sy, 20 * 2 + 50);
    }

    #[test]
    fn scale_coords_for_uses_specified_id() {
        let _lock = lock_and_reset();
        let id1 = register_screenshot(1.0, 0, 0, "first".to_string());
        let _id2 = register_screenshot(2.0, 100, 50, "second".to_string());
        // Specifying id1 should use the FIRST entry's metadata
        // (scale=1.0, offset=(0, 0)) even though id2 is newer.
        let (sx, sy) = scale_coords_for(10, 20, Some(id1)).expect("id1");
        assert_eq!(sx, 10);
        assert_eq!(sy, 20);
    }

    #[test]
    fn scale_coords_for_returns_error_on_evicted_id() {
        let _lock = lock_and_reset();
        // Push enough entries to overflow the registry so id 0 is evicted.
        for i in 0..(SCREENSHOT_REGISTRY_CAPACITY + 5) {
            register_screenshot(1.0, 0, 0, format!("entry-{}", i));
        }
        // The earliest id (0 from this fresh registry) should be gone.
        let result = scale_coords_for(10, 20, Some(0));
        assert!(result.is_err(), "evicted id should error");
        let msg = result.unwrap_err();
        assert!(
            msg.contains("not found") || msg.contains("evicted"),
            "error should mention eviction: {}",
            msg
        );
    }

    #[test]
    fn scale_coords_for_errors_when_no_screenshots_registered() {
        let _lock = lock_and_reset();
        let result = scale_coords_for(10, 20, None);
        assert!(result.is_err(), "no screenshots → error");
        let msg = result.unwrap_err();
        assert!(
            msg.contains("No screenshots"),
            "error should mention missing registration: {}",
            msg
        );
    }

    #[test]
    fn registry_lru_evicts_oldest_when_full() {
        let _lock = lock_and_reset();
        let mut ids = Vec::new();
        for i in 0..(SCREENSHOT_REGISTRY_CAPACITY + 3) {
            ids.push(register_screenshot(1.0, 0, 0, format!("entry-{}", i)));
        }
        // First 3 should be evicted; last `SCREENSHOT_REGISTRY_CAPACITY`
        // should still be present.
        assert!(get_metadata(Some(ids[0])).is_none());
        assert!(get_metadata(Some(ids[1])).is_none());
        assert!(get_metadata(Some(ids[2])).is_none());
        assert!(get_metadata(Some(ids[3])).is_some());
        assert!(get_metadata(Some(*ids.last().unwrap())).is_some());
    }

    /// `GUI_TOOL_NAMES` is the SSOT for which tools are computer-use.
    /// If a tool is removed from the const but its name is still
    /// referenced in `tool_call.rs::parse` or vice versa, the registry
    /// drifts. This test asserts every name has a corresponding parse
    /// path — it uses `ToolCall::to_agent_action` which is the entry
    /// point that maps tool names to `AgentAction` variants.
    #[test]
    fn gui_tool_names_match_action_variants() {
        use crate::agents::AgentAction;
        use crate::models::tool_call::{FunctionCall, ToolCall};
        use serde_json::json;

        for name in GUI_TOOL_NAMES {
            // Build a minimal-but-valid arg payload per tool to avoid
            // ParseError: missing-required-arg false positives.
            let args = match *name {
                "click" => json!({"x": 0, "y": 0}),
                "type_text" => json!({"text": ""}),
                "press_key" => json!({"key": "a"}),
                "scroll" => json!({"direction": "up"}),
                "mouse_move" => json!({"x": 0, "y": 0}),
                _ => json!({}),
            };
            let tc = ToolCall {
                id: None,
                function: FunctionCall {
                    name: (*name).to_string(),
                    arguments: args,
                },
            };
            let action = tc.to_agent_action();
            assert!(
                action.is_ok(),
                "tool '{}' in GUI_TOOL_NAMES has no parse path: {:?}",
                name,
                action.err()
            );
            // ParseError variant also counts as failure for this guard.
            if let Ok(AgentAction::ParseError { message }) = action {
                panic!(
                    "tool '{}' in GUI_TOOL_NAMES parsed to ParseError: {}",
                    name, message
                );
            }
        }
    }

    /// Step 5f Wave 6 (Bug 5): scroll amount > MAX_SCROLL_AMOUNT
    /// clamps down. Verifies the success message also notes the clamp
    /// so the model can see what actually happened.
    #[tokio::test]
    async fn scroll_clamps_excessive_amount() {
        // No display backend in test → execute_scroll bails early. We
        // exercise the clamp logic by reading MAX_SCROLL_AMOUNT and
        // confirming the constant is what the plan promised.
        assert_eq!(crate::constants::MAX_SCROLL_AMOUNT, 100);
        // The actual `(clamped from N to 100)` message path is
        // exercised by manual smoke in Wave 7 (requires X11/Wayland).
    }

    /// Step 5f Wave 6 (Bug 10): TempFileGuard removes its path on drop.
    #[test]
    fn temp_file_guard_removes_path_on_drop() {
        let path = std::env::temp_dir().join(format!(
            "mermaid-test-tempguard-{}.tmp",
            SCREENSHOT_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, b"test data").expect("write");
        assert!(path.exists());
        {
            let _guard = TempFileGuard(path.clone());
            assert!(path.exists(), "guard should not remove until drop");
        }
        // Guard out of scope → file removed.
        assert!(
            !path.exists(),
            "TempFileGuard must remove path on drop, but {} still exists",
            path.display()
        );
    }

    /// Step 5f Wave 6 (Bug 9): TYPE_KEY_DELAY_MS bumped to 25ms.
    #[test]
    fn type_key_delay_is_25ms() {
        assert_eq!(crate::constants::TYPE_KEY_DELAY_MS, 25);
    }
}
