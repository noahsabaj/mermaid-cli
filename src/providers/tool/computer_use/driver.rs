//! `ComputerUseDriver` — the shared backend-dispatch layer for the
//! seven computer-use tools.
//!
//! The driver wraps three things:
//!
//!   1. A `Backend` discriminant (`X11`, `Wayland`, `MacOS`, …). Tools
//!      match on it to pick the right subprocess dispatch.
//!   2. A bounded `ScreenshotRegistry`. Every capture gets a stable
//!      `id`; the model includes that id on later `click(x, y,
//!      screenshot_id)` so coordinate translation uses the right
//!      scale+offset even if the newest screenshot has shifted the
//!      "latest" entry.
//!   3. `ensure_alive()` — a cheap re-probe called at the top of every
//!      tool's `execute`. Catches the case where the display went
//!      away between registration and invocation (detached SSH,
//!      closed lid).
//!
//! Subprocess dispatch uses `tokio::process::Command` so each external
//! binary can race against `ctx.token.cancelled()`. `kill_on_drop(true)`
//! reaps children whose parent future gets cancelled.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::constants::{SCREENSHOT_MAX_WIDTH, SCREENSHOT_REGISTRY_CAPACITY};
use crate::domain::ToolOutcome;

use super::Backend;

/// Per-capture metadata retained so subsequent clicks can translate
/// model-space coords back to screen-space.
#[derive(Debug, Clone)]
pub struct ScreenshotMetadata {
    pub id: u64,
    pub scale_factor: f64,
    pub offset_x: i32,
    pub offset_y: i32,
    /// Human-readable capture kind (`"fullscreen"`, `"focused window"`,
    /// …). Surfaced in error messages if the model references an
    /// evicted id.
    pub kind: String,
}

/// Bounded ring buffer of recent screenshot metadata. Capacity from
/// `constants::SCREENSHOT_REGISTRY_CAPACITY` (= 16). When full, push
/// evicts the oldest; referencing an evicted id fails cleanly with
/// "take a fresh screenshot" rather than silently clicking at wrong
/// coordinates.
#[derive(Debug, Default)]
pub struct ScreenshotRegistry {
    entries: VecDeque<ScreenshotMetadata>,
}

impl ScreenshotRegistry {
    pub fn new() -> Self {
        Self {
            entries: VecDeque::new(),
        }
    }

    pub fn push(&mut self, meta: ScreenshotMetadata) {
        if self.entries.len() >= SCREENSHOT_REGISTRY_CAPACITY {
            self.entries.pop_front();
        }
        self.entries.push_back(meta);
    }

    pub fn get(&self, id: u64) -> Option<&ScreenshotMetadata> {
        self.entries.iter().find(|m| m.id == id)
    }

    pub fn latest(&self) -> Option<&ScreenshotMetadata> {
        self.entries.back()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// What the screenshot tool accepts: which slice of the display to
/// capture.
#[derive(Debug, Clone)]
pub enum ScreenshotSpec {
    Fullscreen,
    Focused,
    Monitor(String),
    /// `(x, y, width, height)` in screen pixels.
    Region(i32, i32, u32, u32),
    Window(String),
}

/// Result of a capture: encoded bytes + registry id + a human-readable
/// summary for the tool's `output` field.
#[derive(Debug)]
pub struct CaptureResult {
    pub id: u64,
    pub base64_png: String,
    pub raw_bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub scale_factor: f64,
    pub offset_x: i32,
    pub offset_y: i32,
    pub summary: String,
}

/// Shared driver all seven computer-use tools hold an `Arc<>` to.
pub struct ComputerUseDriver {
    backend: Backend,
    registry: Mutex<ScreenshotRegistry>,
    /// Monotonic counter for temp file uniqueness. Distinct from the
    /// registry id counter so filenames don't collide across runs
    /// that share temp dir.
    file_counter: AtomicU64,
    /// Monotonic counter for registry ids — stable across process
    /// lifetime, survives evictions.
    id_counter: AtomicU64,
}

impl ComputerUseDriver {
    pub fn new(backend: Backend) -> Self {
        Self {
            backend,
            registry: Mutex::new(ScreenshotRegistry::new()),
            file_counter: AtomicU64::new(0),
            id_counter: AtomicU64::new(0),
        }
    }

    pub fn backend(&self) -> Backend {
        self.backend
    }

    /// Cheap mid-call liveness check. Tools call this first inside
    /// `execute()`; if the display went away after registration,
    /// they return a clean error instead of hanging on subprocess
    /// dispatch.
    pub fn ensure_alive(&self) -> Result<(), ToolOutcome> {
        if super::display_is_reachable(self.backend) {
            Ok(())
        } else {
            Err(ToolOutcome::Error {
                error: format!(
                    "Display unreachable (backend={:?}). Was the session \
                     detached, or did `DISPLAY` change?",
                    self.backend
                ),
                duration_secs: 0.0,
            })
        }
    }

    /// Translate model-space coords to screen-space using the metadata
    /// registered for `screenshot_id` (or the latest if None).
    pub fn scale_coords(
        &self,
        x: i32,
        y: i32,
        screenshot_id: Option<u64>,
    ) -> Result<(i32, i32), String> {
        let reg = self.registry.lock().map_err(|e| e.to_string())?;
        let meta = match screenshot_id {
            Some(id) => reg.get(id).cloned().ok_or_else(|| {
                format!(
                    "Screenshot id {} not found in registry (likely evicted — capacity {}). \
                     Take a fresh screenshot and retry with the new id.",
                    id, SCREENSHOT_REGISTRY_CAPACITY
                )
            })?,
            None => reg.latest().cloned().ok_or_else(|| {
                "No screenshots registered yet — call `screenshot` before \
                 `click` / `mouse_move`."
                    .to_string()
            })?,
        };
        Ok((
            (x as f64 * meta.scale_factor) as i32 + meta.offset_x,
            (y as f64 * meta.scale_factor) as i32 + meta.offset_y,
        ))
    }

    /// Allocate a fresh registry id and record metadata.
    pub fn register_screenshot(
        &self,
        scale_factor: f64,
        offset_x: i32,
        offset_y: i32,
        kind: String,
    ) -> u64 {
        let id = self.id_counter.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut reg) = self.registry.lock() {
            reg.push(ScreenshotMetadata {
                id,
                scale_factor,
                offset_x,
                offset_y,
                kind,
            });
        }
        id
    }

    /// Capture and return the encoded result. Respects cancellation
    /// via `token.cancelled()` races in the subprocess wait.
    pub async fn capture(
        &self,
        spec: ScreenshotSpec,
        token: &CancellationToken,
    ) -> Result<CaptureResult> {
        self.ensure_alive().map_err(|outcome| match outcome {
            ToolOutcome::Error { error, .. } => anyhow::anyhow!(error),
            _ => anyhow::anyhow!("display unreachable"),
        })?;

        let seq = self.file_counter.fetch_add(1, Ordering::Relaxed);
        let temp_path = std::env::temp_dir().join(format!("mermaid-screenshot-{}.png", seq));
        let temp_str = temp_path.to_string_lossy().to_string();
        let _guard = TempFileGuard(temp_path.clone());

        let (offset_x, offset_y, kind) =
            dispatch_capture(self.backend, &spec, &temp_str, token).await?;

        let scale_factor = downscale_if_needed(&temp_str, SCREENSHOT_MAX_WIDTH).await?;
        let id = self.register_screenshot(scale_factor, offset_x, offset_y, kind.clone());

        let raw_bytes = tokio::fs::read(&temp_path)
            .await
            .context("reading captured screenshot")?;
        let width = read_png_width(&raw_bytes).unwrap_or(0);
        let height = read_png_height(&raw_bytes).unwrap_or(0);
        let base64_png = general_purpose::STANDARD.encode(&raw_bytes);

        let offset_info = if offset_x != 0 || offset_y != 0 {
            format!(", offset: +{}+{}", offset_x, offset_y)
        } else {
            String::new()
        };
        let summary = format!(
            "Screenshot captured (id: {}, {}, {}x{}, scale: {:.2}x{})",
            id, kind, width, height, scale_factor, offset_info
        );

        Ok(CaptureResult {
            id,
            base64_png,
            raw_bytes,
            width,
            height,
            scale_factor,
            offset_x,
            offset_y,
            summary,
        })
    }
}

// ───── RAII temp-file cleanup ──────────────────────────────────────

struct TempFileGuard(PathBuf);

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

// ───── subprocess dispatch ─────────────────────────────────────────

async fn dispatch_capture(
    backend: Backend,
    spec: &ScreenshotSpec,
    out_path: &str,
    token: &CancellationToken,
) -> Result<(i32, i32, String)> {
    // Returns (offset_x, offset_y, kind_label). Each branch `select!`s
    // on `token.cancelled()` so Ctrl+C during a slow capture aborts
    // the subprocess cleanly.
    match (backend, spec) {
        (Backend::X11, ScreenshotSpec::Fullscreen) => {
            run_cmd_cancellable(Command::new("scrot").args(["-o", out_path]), token).await?;
            Ok((0, 0, "fullscreen".to_string()))
        },
        (Backend::Wayland, ScreenshotSpec::Fullscreen) => {
            run_cmd_cancellable(Command::new("grim").arg(out_path), token).await?;
            Ok((0, 0, "fullscreen".to_string()))
        },
        (Backend::MacOS, ScreenshotSpec::Fullscreen) => {
            run_cmd_cancellable(Command::new("screencapture").args(["-x", out_path]), token)
                .await?;
            Ok((0, 0, "fullscreen".to_string()))
        },
        (Backend::X11, ScreenshotSpec::Focused) => {
            let (wx, wy) = get_focused_window_geometry_x11()
                .await
                .map(|(x, y, _, _)| (x, y))
                .unwrap_or((0, 0));
            run_cmd_cancellable(Command::new("scrot").args(["-u", "-o", out_path]), token).await?;
            Ok((wx, wy, "focused window".to_string()))
        },
        (Backend::Wayland, ScreenshotSpec::Focused) => anyhow::bail!(
            "Mode 'focused' not supported on Wayland (grim has no focused-window \
             primitive). Use mode: 'fullscreen' or mode: 'monitor' with a specific \
             output name."
        ),
        (Backend::MacOS, ScreenshotSpec::Focused) => {
            run_cmd_cancellable(
                Command::new("screencapture").args(["-x", "-W", out_path]),
                token,
            )
            .await?;
            Ok((0, 0, "focused window".to_string()))
        },
        (Backend::X11, ScreenshotSpec::Region(x, y, w, h)) => {
            run_cmd_cancellable(
                Command::new("scrot").args([
                    "-a",
                    &format!("{},{},{},{}", x, y, w, h),
                    "-o",
                    out_path,
                ]),
                token,
            )
            .await?;
            Ok((*x, *y, format!("region {}x{}+{}+{}", w, h, x, y)))
        },
        (Backend::Wayland, ScreenshotSpec::Region(x, y, w, h)) => {
            run_cmd_cancellable(
                Command::new("grim").args(["-g", &format!("{},{} {}x{}", x, y, w, h), out_path]),
                token,
            )
            .await?;
            Ok((*x, *y, format!("region {}x{}+{}+{}", w, h, x, y)))
        },
        (Backend::X11, ScreenshotSpec::Monitor(name)) => {
            let (mx, my, mw, mh) = parse_monitor_geometry_x11(name).await.ok_or_else(|| {
                anyhow::anyhow!(
                    "Monitor '{}' not found. Run `xrandr --query` to list outputs.",
                    name
                )
            })?;
            run_cmd_cancellable(
                Command::new("scrot").args([
                    "-a",
                    &format!("{},{},{},{}", mx, my, mw, mh),
                    "-o",
                    out_path,
                ]),
                token,
            )
            .await?;
            Ok((mx, my, format!("monitor {}", name)))
        },
        (Backend::Wayland, ScreenshotSpec::Monitor(name)) => {
            run_cmd_cancellable(Command::new("grim").args(["-o", name, out_path]), token).await?;
            Ok((0, 0, format!("monitor {}", name)))
        },
        (Backend::X11, ScreenshotSpec::Window(title)) => {
            // Search for window by name, activate it, sync, then
            // capture the focused window.
            let wid = run_cmd_stdout(Command::new("xdotool").args(["search", "--name", title]))
                .await?
                .lines()
                .next()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "No window found matching '{}'. Use list_windows to see available \
                         windows.",
                        title
                    )
                })?;
            run_cmd_cancellable(
                Command::new("xdotool").args(["windowactivate", "--sync", &wid]),
                token,
            )
            .await?;
            tokio::time::sleep(std::time::Duration::from_millis(
                crate::constants::WINDOW_FOCUS_DELAY_MS,
            ))
            .await;
            let (wx, wy) = get_window_geometry_x11(&wid)
                .await
                .map(|(x, y, _, _)| (x, y))
                .unwrap_or((0, 0));
            run_cmd_cancellable(Command::new("scrot").args(["-u", "-o", out_path]), token).await?;
            Ok((wx, wy, format!("window \"{}\"", title)))
        },
        (Backend::Wayland, ScreenshotSpec::Window(_)) => anyhow::bail!(
            "Mode 'window' not supported on Wayland (grim has no window-by-name capture). \
             Use mode: 'fullscreen' or mode: 'monitor' with a specific output name."
        ),
        (Backend::MacOS, _) => anyhow::bail!(
            "This screenshot mode is not yet ported to macOS. Use mode: 'fullscreen' for now."
        ),
        (Backend::Windows, _) | (Backend::Unsupported, _) => {
            anyhow::bail!("Unsupported platform for computer-use capture")
        },
    }
}

/// Run a `Command` to completion, racing it against cancellation.
/// Relies on `kill_on_drop(true)` reaping the child when the future
/// is dropped on cancel.
pub(crate) async fn run_cmd_cancellable(
    cmd: &mut Command,
    token: &CancellationToken,
) -> Result<()> {
    cmd.kill_on_drop(true);
    tokio::select! {
        biased;
        _ = token.cancelled() => anyhow::bail!("cancelled"),
        res = cmd.output() => {
            let out = res.context("subprocess spawn")?;
            if !out.status.success() {
                anyhow::bail!(
                    "subprocess failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            Ok(())
        }
    }
}

async fn run_cmd_stdout(cmd: &mut Command) -> Result<String> {
    let out = cmd.output().await.context("subprocess spawn")?;
    if !out.status.success() {
        anyhow::bail!(
            "subprocess failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

// ───── geometry helpers (X11 only; Wayland has no equivalent) ──────

async fn get_focused_window_geometry_x11() -> Option<(i32, i32, u32, u32)> {
    let wid = run_cmd_stdout(Command::new("xdotool").arg("getactivewindow"))
        .await
        .ok()?;
    let wid = wid.trim();
    if wid.is_empty() {
        return None;
    }
    get_window_geometry_x11(wid).await
}

async fn get_window_geometry_x11(wid: &str) -> Option<(i32, i32, u32, u32)> {
    let out = run_cmd_stdout(Command::new("xdotool").args(["getwindowgeometry", "--shell", wid]))
        .await
        .ok()?;
    let mut x = None;
    let mut y = None;
    let mut width = None;
    let mut height = None;
    for line in out.lines() {
        if let Some(v) = line.strip_prefix("X=") {
            x = v.parse().ok();
        } else if let Some(v) = line.strip_prefix("Y=") {
            y = v.parse().ok();
        } else if let Some(v) = line.strip_prefix("WIDTH=") {
            width = v.parse().ok();
        } else if let Some(v) = line.strip_prefix("HEIGHT=") {
            height = v.parse().ok();
        }
    }
    Some((x?, y?, width?, height?))
}

async fn parse_monitor_geometry_x11(name: &str) -> Option<(i32, i32, u32, u32)> {
    let out = run_cmd_stdout(Command::new("xrandr").arg("--query"))
        .await
        .ok()?;
    for line in out.lines() {
        if !line.contains(" connected") {
            continue;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.first() != Some(&name) {
            continue;
        }
        for part in &parts[2..] {
            if let Some((res, offsets)) = part.split_once('+')
                && let Some((w, h)) = res.split_once('x')
            {
                let width = w.parse::<u32>().ok()?;
                let height = h.parse::<u32>().ok()?;
                let mut off = offsets.splitn(2, '+');
                let x = off.next()?.parse::<i32>().ok()?;
                let y = off.next()?.parse::<i32>().ok()?;
                return Some((x, y, width, height));
            }
        }
    }
    None
}

// ───── PNG inspection (no image crate dep) ─────────────────────────

fn read_png_width(bytes: &[u8]) -> Option<u32> {
    if bytes.len() > 24 && &bytes[0..8] == b"\x89PNG\r\n\x1a\n" {
        Some(u32::from_be_bytes([
            bytes[16], bytes[17], bytes[18], bytes[19],
        ]))
    } else {
        None
    }
}

fn read_png_height(bytes: &[u8]) -> Option<u32> {
    if bytes.len() > 28 && &bytes[0..8] == b"\x89PNG\r\n\x1a\n" {
        Some(u32::from_be_bytes([
            bytes[20], bytes[21], bytes[22], bytes[23],
        ]))
    } else {
        None
    }
}

/// Downscale the PNG at `path` to at most `max_width` pixels wide,
/// using ImageMagick `convert` or ffmpeg as a fallback. Returns the
/// scale factor (original_width / max_width; 1.0 if no scaling was
/// needed).
async fn downscale_if_needed(path: &str, max_width: u32) -> Result<f64> {
    let bytes = tokio::fs::read(path).await?;
    let original_width = read_png_width(&bytes).unwrap_or(1920);
    if original_width <= max_width {
        return Ok(1.0);
    }
    let scale_factor = original_width as f64 / max_width as f64;
    let scaled = format!("{}.scaled.png", path);

    let convert = Command::new("convert")
        .args([path, "-resize", &format!("{}x", max_width), &scaled])
        .output()
        .await;
    if let Ok(o) = convert
        && o.status.success()
    {
        tokio::fs::rename(&scaled, path).await?;
        return Ok(scale_factor);
    }

    let ffmpeg = Command::new("ffmpeg")
        .args([
            "-y",
            "-i",
            path,
            "-vf",
            &format!("scale={}:-1", max_width),
            &scaled,
        ])
        .output()
        .await;
    if let Ok(o) = ffmpeg
        && o.status.success()
    {
        tokio::fs::rename(&scaled, path).await?;
        return Ok(scale_factor);
    }

    let _ = tokio::fs::remove_file(&scaled).await;
    tracing::warn!(
        original_width,
        "neither ImageMagick nor ffmpeg available; sending full-resolution screenshot"
    );
    Ok(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_lru_evicts_oldest_past_capacity() {
        let mut r = ScreenshotRegistry::new();
        for i in 0..(SCREENSHOT_REGISTRY_CAPACITY as u64 + 3) {
            r.push(ScreenshotMetadata {
                id: i,
                scale_factor: 1.0,
                offset_x: 0,
                offset_y: 0,
                kind: "fullscreen".to_string(),
            });
        }
        assert_eq!(r.len(), SCREENSHOT_REGISTRY_CAPACITY);
        // First 3 should have been evicted.
        assert!(r.get(0).is_none());
        assert!(r.get(1).is_none());
        assert!(r.get(2).is_none());
        // Latest remains.
        assert_eq!(
            r.latest().unwrap().id,
            SCREENSHOT_REGISTRY_CAPACITY as u64 + 2
        );
    }

    #[test]
    fn scale_coords_applies_scale_and_offset() {
        let d = ComputerUseDriver::new(Backend::X11);
        let id = d.register_screenshot(2.0, 100, 50, "fullscreen".to_string());
        let (sx, sy) = d.scale_coords(10, 20, Some(id)).unwrap();
        assert_eq!(sx, 100 + 20);
        assert_eq!(sy, 50 + 40);
    }

    #[test]
    fn scale_coords_errors_on_evicted_id() {
        let d = ComputerUseDriver::new(Backend::X11);
        for _ in 0..(SCREENSHOT_REGISTRY_CAPACITY + 1) {
            d.register_screenshot(1.0, 0, 0, "fullscreen".to_string());
        }
        // id 0 is evicted now.
        let err = d.scale_coords(0, 0, Some(0)).unwrap_err();
        assert!(
            err.contains("evicted"),
            "expected eviction message, got: {}",
            err
        );
    }

    #[test]
    fn scale_coords_errors_with_no_screenshots_yet() {
        let d = ComputerUseDriver::new(Backend::X11);
        let err = d.scale_coords(10, 20, None).unwrap_err();
        assert!(err.contains("No screenshots"));
    }

    #[test]
    fn ensure_alive_fails_on_unsupported_backend() {
        let d = ComputerUseDriver::new(Backend::Unsupported);
        assert!(d.ensure_alive().is_err());
    }
}
