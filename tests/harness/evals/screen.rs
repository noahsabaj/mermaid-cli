//! The window a screen eval opens: a small settings dialog drawn with core
//! X11, on an Xvfb display of the eval's own.
//!
//! It is a real X client, so the model sees it only in screenshots and changes
//! it only with the mouse and keyboard. What the user saves goes to a file
//! outside the project, so a model cannot pass by writing the answer itself.
//!
//! The integration test binary is the app: the harness starts it again with
//! `--exact it::evals::screen_app` and `MERMAID_EVAL_SCREEN_APP` set.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    ChangeGCAux, ConnectionExt as _, CreateGCAux, CreateWindowAux, EventMask, Gcontext, InputFocus,
    KeyButMask, Keycode, Rectangle, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;

/// Which app to open (only `settings` today).
pub const APP_ENV: &str = "MERMAID_EVAL_SCREEN_APP";
/// Where the app writes what the user saved.
pub const SAVE_ENV: &str = "MERMAID_EVAL_SCREEN_SAVE";

/// Where the window sits on the screen, and its parts, in screen pixels.
const WINDOW: Rectangle = Rectangle {
    x: 100,
    y: 100,
    width: 420,
    height: 140,
};
const FIELD: Rectangle = Rectangle {
    x: 90,
    y: 62,
    width: 200,
    height: 30,
};
const OK: Rectangle = Rectangle {
    x: 310,
    y: 62,
    width: 80,
    height: 30,
};

const PANEL: u32 = 0x00d8_d8d8;
const WHITE: u32 = 0x00ff_ffff;
const BLACK: u32 = 0x0000_0000;
const SELECTION: u32 = 0x0033_66cc;

/// The screen size the evals run at. It fits every model's picture limit,
/// so screenshot pixels are screen pixels.
const SIZE: &str = "1280x800x24";

/// Whether a screen eval can run here: Linux with `Xvfb` on the `PATH`.
#[must_use]
pub fn available() -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join("Xvfb").is_file()))
}

/// A private Xvfb display with the app on it. Dropping it ends both.
pub struct Screen {
    xvfb: Child,
    app: Child,
    /// The `DISPLAY` value for the run.
    display: String,
    /// Where the app writes what the user saves.
    save: PathBuf,
}

impl Screen {
    /// Start Xvfb on a free display, then `app` on it, and wait for its
    /// window. `dir` holds the app's output and logs.
    ///
    /// # Errors
    ///
    /// When Xvfb or the app does not come up.
    pub fn start(app: &str, dir: &Path) -> Result<Self, String> {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let log = |name: &str| {
            std::fs::File::create(dir.join(name))
                .map(Stdio::from)
                .unwrap_or_else(|_| Stdio::null())
        };
        // `-displayfd 1`: Xvfb picks a free display and prints its number.
        let mut xvfb = Command::new("Xvfb")
            .args(["-displayfd", "1", "-screen", "0", SIZE, "-nolisten", "tcp"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(log("xvfb.log"))
            .spawn()
            .map_err(|e| format!("starting Xvfb: {e}"))?;
        let stdout = xvfb.stdout.take().expect("piped stdout");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            let _ = BufReader::new(stdout).read_line(&mut line);
            let _ = tx.send(line);
        });
        let number = match rx.recv_timeout(Duration::from_secs(20)) {
            Ok(line) if !line.trim().is_empty() => line.trim().to_string(),
            _ => {
                let _ = xvfb.kill();
                let _ = xvfb.wait();
                return Err(format!(
                    "Xvfb did not report a display; see {}",
                    dir.join("xvfb.log").display()
                ));
            },
        };
        let display = format!(":{number}");
        let save = dir.join("saved.txt");
        let app = Command::new(std::env::current_exe().map_err(|e| e.to_string())?)
            .args([
                "--exact",
                "it::evals::screen_app",
                "--ignored",
                "--nocapture",
            ])
            .env("DISPLAY", &display)
            .env_remove("WAYLAND_DISPLAY")
            .env(APP_ENV, app)
            .env(SAVE_ENV, &save)
            .stdin(Stdio::null())
            .stdout(log("app.log"))
            .stderr(log("app.err"))
            .spawn();
        let app = match app {
            Ok(app) => app,
            Err(e) => {
                let _ = xvfb.kill();
                let _ = xvfb.wait();
                return Err(format!("starting the screen app: {e}"));
            },
        };
        let screen = Self {
            xvfb,
            app,
            display,
            save,
        };
        let started = Instant::now();
        while !ready_marker(&screen.save).exists() {
            if started.elapsed() > Duration::from_secs(30) {
                return Err(format!(
                    "the screen app did not open its window; see {}",
                    dir.join("app.err").display()
                ));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Ok(screen)
    }

    #[must_use]
    pub fn display(&self) -> &str {
        &self.display
    }

    /// What the app saved, if the user saved.
    #[must_use]
    pub fn saved(&self) -> Option<String> {
        std::fs::read_to_string(&self.save).ok()
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        for child in [&mut self.app, &mut self.xvfb] {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// The file that appears once the window is on the screen.
fn ready_marker(save: &Path) -> PathBuf {
    save.with_extension("ready")
}

/// Run the app named by [`APP_ENV`] until the harness kills it. Returns at
/// once when the variable is unset, so a plain test run passes through.
///
/// # Panics
///
/// When the display cannot be opened or drawn on: the eval then fails with
/// the app's stderr in its sandbox.
pub fn serve_from_env() {
    let Ok(app) = std::env::var(APP_ENV) else {
        return;
    };
    assert_eq!(app, "settings", "unknown screen app {app:?}");
    let save = PathBuf::from(std::env::var(SAVE_ENV).expect("MERMAID_EVAL_SCREEN_SAVE"));
    Settings::open(save)
        .and_then(|mut app| app.run())
        .unwrap_or_else(|e| panic!("screen app: {e}"));
}

struct Settings {
    conn: RustConnection,
    window: Window,
    gc: Gcontext,
    save: PathBuf,
    port: String,
    focused: bool,
    selected: bool,
    keymap: Keymap,
    last_click: Option<u32>,
}

type Res<T> = Result<T, Box<dyn std::error::Error>>;

impl Settings {
    fn open(save: PathBuf) -> Res<Self> {
        let (conn, screen) = x11rb::connect(None)?;
        let root = conn.setup().roots[screen].root;
        let window = conn.generate_id()?;
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            window,
            root,
            WINDOW.x,
            WINDOW.y,
            WINDOW.width,
            WINDOW.height,
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            &CreateWindowAux::new()
                .background_pixel(PANEL)
                .override_redirect(1)
                .event_mask(EventMask::EXPOSURE | EventMask::BUTTON_PRESS | EventMask::KEY_PRESS),
        )?;
        let font = conn.generate_id()?;
        conn.open_font(font, b"fixed")?;
        let gc = conn.generate_id()?;
        conn.create_gc(
            gc,
            window,
            &CreateGCAux::new()
                .foreground(BLACK)
                .background(PANEL)
                .font(font),
        )?;
        conn.map_window(window)?;
        conn.flush()?;
        let keymap = Keymap::read(&conn)?;
        Ok(Self {
            conn,
            window,
            gc,
            save,
            port: "8080".to_string(),
            focused: false,
            selected: false,
            keymap,
            last_click: None,
        })
    }

    fn run(&mut self) -> Res<()> {
        let mut ready = false;
        loop {
            match self.conn.wait_for_event()? {
                Event::Expose(_) => {
                    self.draw()?;
                    if !ready {
                        self.conn.set_input_focus(
                            InputFocus::PARENT,
                            self.window,
                            x11rb::CURRENT_TIME,
                        )?;
                        self.conn.flush()?;
                        std::fs::write(ready_marker(&self.save), b"")?;
                        ready = true;
                    }
                },
                Event::ButtonPress(press) if press.detail == 1 => {
                    self.click(press.event_x, press.event_y, press.time)?;
                },
                Event::KeyPress(key) => self.key(key.detail, key.state)?,
                Event::MappingNotify(_) => self.keymap = Keymap::read(&self.conn)?,
                _ => {},
            }
        }
    }

    fn click(&mut self, x: i16, y: i16, time: u32) -> Res<()> {
        let inside = |r: Rectangle| {
            x >= r.x
                && y >= r.y
                && i32::from(x) < i32::from(r.x) + i32::from(r.width)
                && i32::from(y) < i32::from(r.y) + i32::from(r.height)
        };
        let double = self
            .last_click
            .is_some_and(|last| time.wrapping_sub(last) < 500);
        self.last_click = Some(time);
        if inside(FIELD) {
            // A double or triple click selects the whole value.
            self.selected = self.focused && double;
            self.focused = true;
        } else if inside(OK) {
            self.save_settings()?;
        } else {
            self.focused = false;
            self.selected = false;
        }
        self.draw()
    }

    fn key(&mut self, code: Keycode, state: KeyButMask) -> Res<()> {
        if !self.focused {
            return Ok(());
        }
        let control = state.contains(KeyButMask::CONTROL);
        let shift = state.contains(KeyButMask::SHIFT);
        match self.keymap.keysym(code, shift) {
            // Return
            0xff0d => self.save_settings()?,
            // BackSpace
            0xff08 => {
                if self.selected {
                    self.port.clear();
                } else {
                    self.port.pop();
                }
                self.selected = false;
            },
            sym if control && (sym == u32::from(b'a') || sym == u32::from(b'A')) => {
                self.selected = true;
            },
            sym if !control => {
                if let Some(c) = keysym_char(sym) {
                    if self.selected {
                        self.port.clear();
                        self.selected = false;
                    }
                    self.port.push(c);
                }
            },
            _ => {},
        }
        self.draw()
    }

    fn save_settings(&self) -> Res<()> {
        std::fs::write(&self.save, format!("port={}\n", self.port))?;
        Ok(())
    }

    fn fill(&self, color: u32, rect: Rectangle) -> Res<()> {
        self.conn
            .change_gc(self.gc, &ChangeGCAux::new().foreground(color))?;
        self.conn
            .poly_fill_rectangle(self.window, self.gc, &[rect])?;
        Ok(())
    }

    fn text(&self, fg: u32, bg: u32, x: i16, y: i16, text: &str) -> Res<()> {
        self.conn
            .change_gc(self.gc, &ChangeGCAux::new().foreground(fg).background(bg))?;
        self.conn
            .image_text8(self.window, self.gc, x, y, text.as_bytes())?;
        Ok(())
    }

    fn draw(&self) -> Res<()> {
        self.fill(
            PANEL,
            Rectangle {
                x: 0,
                y: 0,
                ..WINDOW
            },
        )?;
        self.text(BLACK, PANEL, 20, 30, "Settings")?;
        self.text(BLACK, PANEL, 20, 82, "Port:")?;
        self.fill(WHITE, FIELD)?;
        self.conn
            .change_gc(self.gc, &ChangeGCAux::new().foreground(BLACK))?;
        let border = if self.focused { 2 } else { 1 };
        for inset in 0..border {
            self.conn.poly_rectangle(
                self.window,
                self.gc,
                &[Rectangle {
                    x: FIELD.x + inset,
                    y: FIELD.y + inset,
                    width: FIELD.width - 1 - 2 * inset.unsigned_abs(),
                    height: FIELD.height - 1 - 2 * inset.unsigned_abs(),
                }],
            )?;
        }
        let (fg, bg) = if self.selected {
            (WHITE, SELECTION)
        } else {
            (BLACK, WHITE)
        };
        self.text(fg, bg, FIELD.x + 8, FIELD.y + 20, &self.port)?;
        self.fill(PANEL - 0x0018_1818, OK)?;
        self.conn
            .change_gc(self.gc, &ChangeGCAux::new().foreground(BLACK))?;
        self.conn.poly_rectangle(
            self.window,
            self.gc,
            &[Rectangle {
                width: OK.width - 1,
                height: OK.height - 1,
                ..OK
            }],
        )?;
        self.text(BLACK, PANEL - 0x0018_1818, OK.x + 34, OK.y + 20, "OK")?;
        self.conn.flush()?;
        Ok(())
    }
}

/// The keyboard mapping, read again whenever it changes: Mermaid's X11
/// backend maps spare keycodes to the characters it types.
struct Keymap {
    min: u8,
    per_code: u8,
    syms: Vec<u32>,
}

impl Keymap {
    fn read(conn: &RustConnection) -> Res<Self> {
        let setup = conn.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let reply = conn.get_keyboard_mapping(min, max - min + 1)?.reply()?;
        Ok(Self {
            min,
            per_code: reply.keysyms_per_keycode,
            syms: reply.keysyms,
        })
    }

    fn keysym(&self, code: Keycode, shift: bool) -> u32 {
        let per = usize::from(self.per_code);
        let base = usize::from(code.saturating_sub(self.min)) * per;
        let plain = self.syms.get(base).copied().unwrap_or(0);
        let shifted = self.syms.get(base + 1).copied().filter(|&s| s != 0);
        match (shift, shifted) {
            (true, Some(sym)) => sym,
            _ => plain,
        }
    }
}

/// The character a keysym types, for Latin-1 and Unicode keysyms.
fn keysym_char(sym: u32) -> Option<char> {
    match sym {
        0x20..=0x7e | 0xa0..=0xff => char::from_u32(sym),
        0x0100_0000..=0x0110_ffff => char::from_u32(sym - 0x0100_0000),
        _ => None,
    }
}
