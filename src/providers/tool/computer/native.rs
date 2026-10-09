//! The Windows and macOS screen: xcap takes the picture, enigo sends the
//! mouse and keyboard input. Both are pure Rust on these platforms.

use enigo::{Axis, Coordinate, Direction, Enigo, Keyboard, Mouse, Settings};
use xcap::Monitor;

use super::desktop::{Button, Desktop, Key};

/// What the user must allow before Mermaid can see or drive the screen.
#[cfg(target_os = "macos")]
const CAPTURE_HINT: &str = "; macOS needs Screen Recording allowed for this terminal (System Settings > Privacy & Security > Screen Recording)";
#[cfg(not(target_os = "macos"))]
const CAPTURE_HINT: &str = "";
#[cfg(target_os = "macos")]
const INPUT_HINT: &str = "; macOS needs Accessibility allowed for this terminal (System Settings > Privacy & Security > Accessibility)";
#[cfg(not(target_os = "macos"))]
const INPUT_HINT: &str = "";

fn capture_err(e: impl std::fmt::Display) -> String {
    format!("screen capture failed: {e}{CAPTURE_HINT}")
}

fn input_err(e: impl std::fmt::Display) -> String {
    format!("input failed: {e}{INPUT_HINT}")
}

pub(super) struct Native {
    enigo: Enigo,
    monitor: Monitor,
}

impl Native {
    pub(super) fn connect() -> Result<Self, String> {
        let monitors = Monitor::all().map_err(capture_err)?;
        let primary = monitors
            .iter()
            .position(|m| m.is_primary().unwrap_or(false))
            .unwrap_or(0);
        let monitor = monitors
            .into_iter()
            .nth(primary)
            .ok_or_else(|| capture_err("no screen found"))?;
        let enigo = Enigo::new(&Settings {
            release_keys_when_dropped: true,
            ..Settings::default()
        })
        .map_err(input_err)?;
        Ok(Self { enigo, monitor })
    }
}

fn direction(down: bool) -> Direction {
    if down {
        Direction::Press
    } else {
        Direction::Release
    }
}

fn key(key: Key) -> Result<enigo::Key, String> {
    use enigo::Key as E;
    Ok(match key {
        Key::Char(c) => E::Unicode(c),
        Key::Return => E::Return,
        Key::Tab => E::Tab,
        Key::Escape => E::Escape,
        Key::Backspace => E::Backspace,
        Key::Delete => E::Delete,
        #[cfg(target_os = "windows")]
        Key::Insert => E::Insert,
        #[cfg(target_os = "macos")]
        Key::Insert => return Err("a Mac keyboard has no Insert key".to_string()),
        Key::Home => E::Home,
        Key::End => E::End,
        Key::PageUp => E::PageUp,
        Key::PageDown => E::PageDown,
        Key::Left => E::LeftArrow,
        Key::Up => E::UpArrow,
        Key::Right => E::RightArrow,
        Key::Down => E::DownArrow,
        Key::Space => E::Space,
        Key::F(n) => {
            const F: [enigo::Key; 20] = [
                E::F1,
                E::F2,
                E::F3,
                E::F4,
                E::F5,
                E::F6,
                E::F7,
                E::F8,
                E::F9,
                E::F10,
                E::F11,
                E::F12,
                E::F13,
                E::F14,
                E::F15,
                E::F16,
                E::F17,
                E::F18,
                E::F19,
                E::F20,
            ];
            F.get(usize::from(n).wrapping_sub(1))
                .copied()
                .ok_or_else(|| format!("unknown key F{n}"))?
        },
        Key::Shift => E::Shift,
        Key::Control => E::Control,
        Key::Alt => E::Alt,
        Key::Super => E::Meta,
        Key::CapsLock => E::CapsLock,
    })
}

impl Desktop for Native {
    fn capture(&mut self) -> Result<image::RgbaImage, String> {
        self.monitor.capture_image().map_err(capture_err)
    }

    fn input_size(&mut self) -> Result<(u32, u32), String> {
        let (w, h) = self.enigo.main_display().map_err(input_err)?;
        Ok((w.max(1) as u32, h.max(1) as u32))
    }

    fn move_to(&mut self, x: i32, y: i32) -> Result<(), String> {
        self.enigo
            .move_mouse(x, y, Coordinate::Abs)
            .map_err(input_err)
    }

    fn cursor(&mut self) -> Result<(i32, i32), String> {
        self.enigo.location().map_err(input_err)
    }

    fn button(&mut self, button: Button, down: bool) -> Result<(), String> {
        let button = match button {
            Button::Left => enigo::Button::Left,
            Button::Middle => enigo::Button::Middle,
            Button::Right => enigo::Button::Right,
        };
        self.enigo
            .button(button, direction(down))
            .map_err(input_err)
    }

    fn scroll(&mut self, dx: i32, dy: i32) -> Result<(), String> {
        if dy != 0 {
            self.enigo.scroll(dy, Axis::Vertical).map_err(input_err)?;
        }
        if dx != 0 {
            self.enigo.scroll(dx, Axis::Horizontal).map_err(input_err)?;
        }
        Ok(())
    }

    fn key(&mut self, k: Key, down: bool) -> Result<(), String> {
        self.enigo.key(key(k)?, direction(down)).map_err(input_err)
    }

    fn text(&mut self, text: &str) -> Result<(), String> {
        self.enigo.text(text).map_err(input_err)
    }
}
