//! Computer-use tools — screenshot capture, mouse + keyboard control.
//!
//! Seven tools total (`screenshot`, `click`, `type_text`, `press_key`,
//! `scroll`, `mouse_move`, `list_windows`) share one `ComputerUseDriver`.
//! The driver owns the platform-specific subprocess dispatch (scrot/
//! xdotool on X11, grim/ydotool on Wayland, screencapture/cliclick on
//! macOS) and the `ScreenshotRegistry` — a small LRU buffer of recent
//! capture metadata so the model can pass `screenshot_id` on
//! `click`/`mouse_move` to lock coordinates to a specific capture.
//!
//! Registration is gated two ways:
//! - `TuiMode::Headless` (`mermaid run <prompt>`) never registers any
//!   computer-use tool regardless of what the display probes say —
//!   a CI job has no user to watch a screenshot.
//! - `Backend::probe()` runs an eager capability check at startup
//!   (env vars + required binaries + `xdpyinfo` smoke test). If the
//!   result is `Unsupported`, no tools register.
//!
//! The driver ALSO exposes `ensure_alive()` which every tool calls at
//! the top of `execute`. It's a cheap re-probe that catches the
//! "`DISPLAY=:0` ghost" case: env looks right, binaries exist, but
//! the X server is actually unreachable (SSH forwarding without an
//! X server, detached display, laptop lid closed).

pub mod click;
pub mod driver;
pub mod list_windows;
pub mod mouse_move;
pub mod press_key;
pub mod screenshot;
pub mod scroll;
pub mod type_text;

use std::process::Command;

use serde_json::Value;

use crate::domain::{ToolMetadata, ToolOutcome, ToolRunMetadata};
use crate::providers::ctx::{ExecContext, ProgressEvent};

pub use click::ClickTool;
pub use driver::ComputerUseDriver;
pub use list_windows::ListWindowsTool;
pub use mouse_move::MouseMoveTool;
pub use press_key::PressKeyTool;
pub use screenshot::ScreenshotTool;
pub use scroll::ScrollTool;
pub use type_text::TypeTextTool;

/// Platform / display-server the driver dispatches to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    X11,
    Wayland,
    MacOS,
    Windows,
    Unsupported,
}

impl Backend {
    /// Whether the driver has any tools it can run on this backend.
    pub fn is_usable(self) -> bool {
        !matches!(self, Backend::Unsupported)
    }

    /// Whether this backend can inject pointer + keyboard events (click,
    /// type_text, press_key, scroll, mouse_move). X11 (xdotool) and Wayland
    /// (ydotool/wtype) only; macOS capture works via `screencapture`, but the
    /// input verbs are unimplemented and `bail!` in the driver, so they must
    /// not be advertised there (#35). Windows is a stub.
    pub fn supports_input_injection(self) -> bool {
        matches!(self, Backend::X11 | Backend::Wayland)
    }

    /// Whether this backend can enumerate windows (`list_windows`). X11 only —
    /// via `xdotool search`; Wayland has no portable primitive and the driver
    /// `bail!`s (#35).
    pub fn supports_window_listing(self) -> bool {
        matches!(self, Backend::X11)
    }
}

/// Eager probe. Runs at startup to decide registration — does the
/// right binary exist? Is the display reachable? Returns
/// `Backend::Unsupported` when mermaid can't drive the display even
/// though env vars might suggest otherwise (e.g. SSH forwarding).
pub fn probe() -> Backend {
    if cfg!(target_os = "macos") {
        if has_command("screencapture") {
            return Backend::MacOS;
        }
        return Backend::Unsupported;
    }
    if cfg!(target_os = "windows") {
        // Windows backend is a v0.6 stub — not wired here. Once a
        // real impl lands, probe PowerShell / SendInput here.
        return Backend::Unsupported;
    }

    // Linux: try Wayland first (prefer if both are set).
    if std::env::var("WAYLAND_DISPLAY").is_ok()
        && has_command("grim")
        && (has_command("ydotool") || has_command("wtype"))
    {
        return Backend::Wayland;
    }

    // Linux: fall back to X11. The xdpyinfo probe catches the ghost
    // case — DISPLAY is set but no X server responds (common over
    // SSH without X forwarding, or after a stale SSH reconnect).
    if std::env::var("DISPLAY").is_ok()
        && has_command("scrot")
        && has_command("xdotool")
        && xdpyinfo_alive()
    {
        return Backend::X11;
    }

    Backend::Unsupported
}

/// Quick re-probe used by `ComputerUseDriver::ensure_alive`. Cheaper
/// than the full `probe()` — just checks the display answers — so
/// every tool call can afford it.
pub fn display_is_reachable(backend: Backend) -> bool {
    match backend {
        Backend::X11 => xdpyinfo_alive(),
        Backend::Wayland => std::env::var("WAYLAND_DISPLAY").is_ok(),
        Backend::MacOS | Backend::Windows => true,
        Backend::Unsupported => false,
    }
}

pub(super) fn has_command(name: &str) -> bool {
    // `which` returns 0 iff the binary is on PATH. Cheap and universal
    // across Linux + macOS; Windows would want `where.exe` but
    // computer-use on Windows is stubbed out anyway.
    Command::new("which")
        .arg(name)
        .output()
        .map(|o| o.status.success() && !o.stdout.is_empty())
        .unwrap_or(false)
}

/// Exit-0 check on `xdpyinfo` with a 200ms timeout. This is the
/// difference between "`DISPLAY` is set" and "an X server will
/// actually answer us."
fn xdpyinfo_alive() -> bool {
    if !has_command("xdpyinfo") {
        // Some minimal X setups don't ship xdpyinfo. Fall back to a
        // `xdotool getactivewindow` probe (we already require
        // xdotool for clicks anyway).
        return Command::new("xdotool")
            .arg("getactivewindow")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
    }
    // Use a timeout wrapper so a wedged display doesn't hang startup.
    match Command::new("timeout").arg("0.2").arg("xdpyinfo").output() {
        Ok(o) => o.status.success(),
        Err(_) => {
            // `timeout` not available (macOS older versions). Fall
            // back to a direct call — shouldn't happen on Linux X11.
            Command::new("xdpyinfo")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        },
    }
}

pub(super) fn computer_use_success(
    action: &'static str,
    params: Value,
    output: String,
    duration_secs: f64,
) -> ToolOutcome {
    ToolOutcome::success(output, format!("{} completed", action), duration_secs).with_metadata(
        ToolRunMetadata {
            detail: ToolMetadata::ComputerUse {
                action: action.to_string(),
                params,
            },
            ..ToolRunMetadata::default()
        },
    )
}

/// Shared post-action auto-screenshot for click / type_text / press_key.
///
/// When `computer_use.auto_screenshot` is enabled, captures the focused window,
/// emits an inline `Artifact` preview on the progress channel, and returns
/// `(summary, base64_png)` for the caller to fold into its outcome. Returns
/// `None` when the flag is off OR the best-effort capture failed — callers then
/// build a screenshot-less outcome (#98). Gating lives here so the three tools
/// share one decision point rather than three byte-identical blocks.
pub(super) async fn emit_auto_screenshot(
    driver: &ComputerUseDriver,
    ctx: &ExecContext,
    caption: &'static str,
) -> Option<(String, String)> {
    if !ctx.config.computer_use.auto_screenshot {
        return None;
    }
    let (summary, base64_png) = driver.capture_focused_for_autoshot(&ctx.token).await?;
    if let Ok(bytes) =
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &base64_png)
    {
        let _ = ctx
            .progress
            .send(ProgressEvent::Artifact {
                mime: "image/png".to_string(),
                data: bytes,
                caption: Some(caption.to_string()),
            })
            .await;
    }
    Some((summary, base64_png))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_unsupported_is_not_usable() {
        assert!(!Backend::Unsupported.is_usable());
        assert!(Backend::X11.is_usable());
        assert!(Backend::Wayland.is_usable());
        assert!(Backend::MacOS.is_usable());
    }

    #[test]
    fn input_injection_only_on_linux_backends() {
        assert!(Backend::X11.supports_input_injection());
        assert!(Backend::Wayland.supports_input_injection());
        assert!(!Backend::MacOS.supports_input_injection());
        assert!(!Backend::Windows.supports_input_injection());
        assert!(!Backend::Unsupported.supports_input_injection());
    }

    #[test]
    fn window_listing_only_on_x11() {
        assert!(Backend::X11.supports_window_listing());
        assert!(!Backend::Wayland.supports_window_listing());
        assert!(!Backend::MacOS.supports_window_listing());
    }

    #[test]
    fn probe_does_not_panic_on_headless() {
        // In the test runner (no DISPLAY, no WAYLAND_DISPLAY on most
        // CI envs), probe() must return Unsupported without panicking.
        // We don't assert a specific result because dev machines may
        // have a live display.
        let _ = probe();
    }

    #[tokio::test]
    async fn auto_screenshot_is_noop_when_disabled() {
        use crate::domain::{ToolCallId, TurnId};
        let mut cfg = crate::app::Config::default();
        cfg.computer_use.auto_screenshot = false;
        let (tx, mut rx) = tokio::sync::mpsc::channel::<ProgressEvent>(8);
        let ctx = ExecContext::new(
            tokio_util::sync::CancellationToken::new(),
            tx,
            ToolCallId(1),
            TurnId(1),
            std::path::PathBuf::from("/tmp"),
            std::sync::Arc::new(cfg),
            String::new(),
            None,
            None,
            None,
            crate::runtime::SafetyMode::FullAccess,
            None,
            None,
            None,
            None,
            None,
        );
        // Backend is irrelevant — the flag short-circuits before any capture.
        let driver = ComputerUseDriver::new(Backend::Unsupported);
        assert!(emit_auto_screenshot(&driver, &ctx, "test").await.is_none());
        assert!(rx.try_recv().is_err(), "no artifact emitted when disabled");
    }
}
