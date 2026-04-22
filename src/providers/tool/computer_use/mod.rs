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

pub mod driver;
pub mod screenshot;

use std::path::Path;
use std::process::Command;

pub use driver::ComputerUseDriver;
pub use screenshot::ScreenshotTool;

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

fn has_command(name: &str) -> bool {
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

/// Utility: strip to filename-safe path for temp files.
#[allow(dead_code)]
pub(crate) fn path_stem(p: &Path) -> String {
    p.file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "unknown".to_string())
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
    fn probe_does_not_panic_on_headless() {
        // In the test runner (no DISPLAY, no WAYLAND_DISPLAY on most
        // CI envs), probe() must return Unsupported without panicking.
        // We don't assert a specific result because dev machines may
        // have a live display.
        let _ = probe();
    }
}
