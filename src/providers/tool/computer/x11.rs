//! The Linux screen: X11, spoken directly through x11rb's pure-Rust
//! connection. Screenshots read the root window; input goes through the
//! XTEST extension, the way `xdotool` sends it. No C library is linked.
//!
//! Wayland is not supported yet: under XWayland the root window shows only
//! X11 programs, so a Wayland session is refused rather than half driven.

use std::time::Duration;

use x11rb::connection::{Connection, RequestConnection as _};
use x11rb::protocol::xproto::{
    BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, ConnectionExt as _, ImageFormat, ImageOrder,
    KEY_PRESS_EVENT, KEY_RELEASE_EVENT, Keycode, Keysym, MOTION_NOTIFY_EVENT, Window,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use super::desktop::{Button, Desktop, Key};

/// Time for X clients to see a changed key mapping before a key that uses it
/// is pressed, and before the mapping changes again.
const REMAP_SETTLE: Duration = Duration::from_millis(20);

pub(super) fn availability() -> Result<(), String> {
    let set = |name| std::env::var_os(name).is_some_and(|v| !v.is_empty());
    if set("WAYLAND_DISPLAY") {
        return Err(
            "computer does not support Wayland yet; it needs an X11 session (DISPLAY set, \
             WAYLAND_DISPLAY unset)"
                .to_string(),
        );
    }
    if !set("DISPLAY") {
        return Err("computer needs a screen: DISPLAY is not set".to_string());
    }
    Ok(())
}

fn x_err(e: impl std::fmt::Display) -> String {
    format!("X11: {e}")
}

pub(super) struct X11 {
    conn: RustConnection,
    root: Window,
    size: (u16, u16),
    /// The keyboard map: `per` keysyms for each keycode from `min_keycode`.
    min_keycode: Keycode,
    per: u8,
    keysyms: Vec<Keysym>,
}

impl X11 {
    pub(super) fn connect() -> Result<Self, String> {
        let (conn, screen) = x11rb::connect(None).map_err(x_err)?;
        let (root, size, min_keycode, max_keycode) = {
            let setup = conn.setup();
            let s = &setup.roots[screen];
            (
                s.root,
                (s.width_in_pixels, s.height_in_pixels),
                setup.min_keycode,
                setup.max_keycode,
            )
        };
        if conn
            .extension_information(x11rb::protocol::xtest::X11_EXTENSION_NAME)
            .map_err(x_err)?
            .is_none()
        {
            return Err("X11: the XTEST extension is missing, so input cannot be sent".to_string());
        }
        let map = conn
            .get_keyboard_mapping(min_keycode, max_keycode - min_keycode + 1)
            .map_err(x_err)?
            .reply()
            .map_err(x_err)?;
        Ok(Self {
            conn,
            root,
            size,
            min_keycode,
            per: map.keysyms_per_keycode,
            keysyms: map.keysyms,
        })
    }

    fn fake(&self, kind: u8, detail: u8, x: i16, y: i16) -> Result<(), String> {
        self.conn
            .xtest_fake_input(kind, detail, x11rb::CURRENT_TIME, self.root, x, y, 0)
            .map_err(x_err)?;
        self.conn.flush().map_err(x_err)
    }

    fn tap_code(&self, code: Keycode, down: bool) -> Result<(), String> {
        let kind = if down {
            KEY_PRESS_EVENT
        } else {
            KEY_RELEASE_EVENT
        };
        self.fake(kind, code, 0, 0)
    }

    /// The keycode that makes `sym`, and whether it needs Shift.
    fn find(&self, sym: Keysym) -> Option<(Keycode, bool)> {
        let per = usize::from(self.per);
        self.keysyms.chunks(per).enumerate().find_map(|(i, row)| {
            let code = self.min_keycode + i as u8;
            row.iter()
                .take(2)
                .position(|&s| s == sym)
                .map(|level| (code, level == 1))
        })
    }

    /// A keycode no key uses, to map a keysym the keyboard lacks onto. The
    /// mapping stays, so a character typed again reuses its keycode, and two
    /// characters never share one while a program may still be reading the
    /// first.
    fn spare(&self) -> Option<Keycode> {
        let per = usize::from(self.per);
        self.keysyms
            .chunks(per)
            .enumerate()
            .rev()
            .find(|(_, row)| row.iter().all(|&s| s == 0))
            .map(|(i, _)| self.min_keycode + i as u8)
    }

    /// Map `sym` onto `code` at every level, so it needs no modifier.
    fn remap(&mut self, code: Keycode, sym: Keysym) -> Result<(), String> {
        let row = vec![sym; usize::from(self.per)];
        self.conn
            .change_keyboard_mapping(1, code, self.per, &row)
            .map_err(x_err)?;
        self.conn.sync().map_err(x_err)?;
        let start = usize::from(code - self.min_keycode) * usize::from(self.per);
        self.keysyms[start..start + row.len()].copy_from_slice(&row);
        std::thread::sleep(REMAP_SETTLE);
        Ok(())
    }

    /// The keycode for `sym`, remapping a spare one when no key makes it.
    fn code_for(&mut self, sym: Keysym) -> Result<(Keycode, bool), String> {
        if let Some(found) = self.find(sym) {
            return Ok(found);
        }
        let code = self
            .spare()
            .ok_or_else(|| format!("X11: no free keycode to type keysym {sym:#x}"))?;
        self.remap(code, sym)?;
        Ok((code, false))
    }

    fn shift_code(&mut self) -> Result<Keycode, String> {
        self.code_for(SHIFT_L).map(|(code, _)| code)
    }

    fn press_sym(&mut self, sym: Keysym, down: bool) -> Result<(), String> {
        let (code, _) = self.code_for(sym)?;
        self.tap_code(code, down)
    }
}

const SHIFT_L: Keysym = 0xffe1;

fn keysym(key: Key) -> Keysym {
    match key {
        Key::Char(c) => char_keysym(c),
        Key::Return => 0xff0d,
        Key::Tab => 0xff09,
        Key::Escape => 0xff1b,
        Key::Backspace => 0xff08,
        Key::Delete => 0xffff,
        Key::Insert => 0xff63,
        Key::Home => 0xff50,
        Key::End => 0xff57,
        Key::PageUp => 0xff55,
        Key::PageDown => 0xff56,
        Key::Left => 0xff51,
        Key::Up => 0xff52,
        Key::Right => 0xff53,
        Key::Down => 0xff54,
        Key::Space => 0x20,
        Key::F(n) => 0xffbd + Keysym::from(n),
        Key::Shift => SHIFT_L,
        Key::Control => 0xffe3,
        Key::Alt => 0xffe9,
        Key::Super => 0xffeb,
        Key::CapsLock => 0xffe5,
    }
}

/// The keysym that types `c`: Latin-1 maps directly, other Unicode is
/// `0x01000000 + codepoint`.
fn char_keysym(c: char) -> Keysym {
    match c {
        '\n' | '\r' => 0xff0d,
        '\t' => 0xff09,
        ' '..='~' | '\u{a0}'..='\u{ff}' => c as Keysym,
        _ => 0x0100_0000 + c as Keysym,
    }
}

impl Desktop for X11 {
    fn capture(&mut self) -> Result<image::RgbaImage, String> {
        let (w, h) = self.size;
        let reply = self
            .conn
            .get_image(ImageFormat::Z_PIXMAP, self.root, 0, 0, w, h, !0)
            .map_err(x_err)?
            .reply()
            .map_err(x_err)?;
        let setup = self.conn.setup();
        let bpp = setup
            .pixmap_formats
            .iter()
            .find(|f| f.depth == reply.depth)
            .map_or(0, |f| f.bits_per_pixel);
        if bpp != 32 || reply.depth < 24 {
            return Err(format!(
                "X11: the screen is {}-bit at {bpp} bits per pixel; only 24-bit color is supported",
                reply.depth
            ));
        }
        let lsb = setup.image_byte_order == ImageOrder::LSB_FIRST;
        let mut rgba = Vec::with_capacity(reply.data.len());
        for px in reply.data.as_chunks::<4>().0 {
            let (r, g, b) = if lsb {
                (px[2], px[1], px[0])
            } else {
                (px[1], px[2], px[3])
            };
            rgba.extend_from_slice(&[r, g, b, 255]);
        }
        image::RgbaImage::from_raw(u32::from(w), u32::from(h), rgba)
            .ok_or_else(|| "X11: the screen image is the wrong size".to_string())
    }

    fn input_size(&mut self) -> Result<(u32, u32), String> {
        Ok((u32::from(self.size.0), u32::from(self.size.1)))
    }

    fn move_to(&mut self, x: i32, y: i32) -> Result<(), String> {
        let clamp = |v: i32| v.clamp(0, i32::from(i16::MAX)) as i16;
        self.fake(MOTION_NOTIFY_EVENT, 0, clamp(x), clamp(y))
    }

    fn cursor(&mut self) -> Result<(i32, i32), String> {
        let reply = self
            .conn
            .query_pointer(self.root)
            .map_err(x_err)?
            .reply()
            .map_err(x_err)?;
        Ok((i32::from(reply.root_x), i32::from(reply.root_y)))
    }

    fn button(&mut self, button: Button, down: bool) -> Result<(), String> {
        let detail = match button {
            Button::Left => 1,
            Button::Middle => 2,
            Button::Right => 3,
        };
        let kind = if down {
            BUTTON_PRESS_EVENT
        } else {
            BUTTON_RELEASE_EVENT
        };
        self.fake(kind, detail, 0, 0)
    }

    fn scroll(&mut self, dx: i32, dy: i32) -> Result<(), String> {
        // Wheel clicks are buttons 4 (up), 5 (down), 6 (left), 7 (right).
        for (amount, back, forward) in [(dy, 4, 5), (dx, 6, 7)] {
            let detail = if amount < 0 { back } else { forward };
            for _ in 0..amount.unsigned_abs() {
                self.fake(BUTTON_PRESS_EVENT, detail, 0, 0)?;
                self.fake(BUTTON_RELEASE_EVENT, detail, 0, 0)?;
            }
        }
        Ok(())
    }

    fn key(&mut self, key: Key, down: bool) -> Result<(), String> {
        self.press_sym(keysym(key), down)
    }

    fn text(&mut self, text: &str) -> Result<(), String> {
        for c in text.chars() {
            let (code, shifted) = self.code_for(char_keysym(c))?;
            let shift = if shifted {
                Some(self.shift_code()?)
            } else {
                None
            };
            if let Some(shift) = shift {
                self.tap_code(shift, true)?;
            }
            self.tap_code(code, true)?;
            self.tap_code(code, false)?;
            if let Some(shift) = shift {
                self.tap_code(shift, false)?;
            }
        }
        self.conn.sync().map_err(x_err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn characters_map_to_their_keysyms() {
        assert_eq!(char_keysym('a'), 0x61);
        assert_eq!(char_keysym('é'), 0xe9);
        assert_eq!(char_keysym('\n'), 0xff0d);
        assert_eq!(char_keysym('€'), 0x0100_20ac);
        assert_eq!(keysym(Key::F(1)), 0xffbe);
        assert_eq!(keysym(Key::F(12)), 0xffc9);
    }

    /// Drives a real X server. Run under a virtual one:
    /// `xvfb-run -s "-screen 0 1280x800x24" cargo nextest run --run-ignored only x11_`.
    #[test]
    #[ignore = "needs an X server"]
    fn x11_captures_moves_and_maps_keys_on_a_real_server() {
        let mut x = X11::connect().expect("an X server on DISPLAY");
        let (w, h) = x.input_size().unwrap();
        let picture = x.capture().unwrap();
        assert_eq!(picture.dimensions(), (w, h));
        x.move_to(100, 50).unwrap();
        assert_eq!(x.cursor().unwrap(), (100, 50));
        x.button(Button::Left, true).unwrap();
        x.button(Button::Left, false).unwrap();
        x.scroll(0, 2).unwrap();
        // A character no keyboard has gets a spare keycode, and keeps it.
        let (code, shifted) = x.code_for(char_keysym('\u{2603}')).unwrap();
        assert!(!shifted);
        assert_eq!(x.find(char_keysym('\u{2603}')), Some((code, false)));
        x.key(Key::Control, true).unwrap();
        x.key(Key::Control, false).unwrap();
    }

    /// What a program sees when the tool types: a window takes the focus and
    /// decodes each key press back to a keysym.
    #[test]
    #[ignore = "needs an X server"]
    fn x11_typed_text_reaches_a_window() {
        use x11rb::protocol::Event;
        use x11rb::protocol::xproto::{CreateWindowAux, EventMask, InputFocus, WindowClass};

        let (app, screen) = x11rb::connect(None).expect("an X server on DISPLAY");
        let root = app.setup().roots[screen].root;
        let window = app.generate_id().unwrap();
        app.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            window,
            root,
            0,
            0,
            200,
            100,
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            &CreateWindowAux::new().event_mask(EventMask::KEY_PRESS),
        )
        .unwrap();
        app.map_window(window).unwrap();
        app.sync().unwrap();
        app.set_input_focus(InputFocus::POINTER_ROOT, window, x11rb::CURRENT_TIME)
            .unwrap();
        app.sync().unwrap();

        let typed = "aB:\u{e9}\u{2603}";
        let mut x = X11::connect().unwrap();
        x.text(typed).unwrap();

        let (min, max) = (app.setup().min_keycode, app.setup().max_keycode);
        let mut seen = String::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while seen.chars().count() < typed.chars().count() && std::time::Instant::now() < deadline {
            let Some(event) = app.poll_for_event().unwrap() else {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            };
            if let Event::KeyPress(press) = event {
                let map = app
                    .get_keyboard_mapping(min, max - min + 1)
                    .unwrap()
                    .reply()
                    .unwrap();
                let per = usize::from(map.keysyms_per_keycode);
                let row = &map.keysyms[usize::from(press.detail - min) * per..][..per];
                let shifted = u16::from(press.state) & 1 == 1;
                let sym = if shifted && row[1] != 0 {
                    row[1]
                } else {
                    row[0]
                };
                if sym == SHIFT_L {
                    continue;
                }
                let code = if sym >= 0x0100_0000 {
                    sym - 0x0100_0000
                } else {
                    sym
                };
                seen.push(char::from_u32(code).unwrap());
            }
        }
        assert_eq!(seen, typed);
    }
}
