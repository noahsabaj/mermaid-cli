//! `computer` — screenshots, mouse and keyboard on the user's real screen.
//!
//! The harness does only the physics: take a picture, fit it to the size a
//! model takes, scale the model's coordinates back to the screen, send the
//! input, and wait a moment for the screen to change. It has no window list,
//! no element finder and no procedure for the model to follow. The actions
//! and their fields are those of Anthropic's computer toolset, which Claude
//! is trained on; `native_tools.rs` rewrites a toolset call onto this tool,
//! and other vision models call it directly. OpenAI's computer tool sends a
//! list of actions and wants the screen back after them: it arrives as the
//! `batch` action, which no schema offers.
//!
//! Screenshots, `zoom` and `cursor_position` only look, so they run in every
//! safety mode once the user has turned the tool on. Every other action goes
//! through the policy gate as `ToolCategory::Computer`.

mod desktop;
#[cfg(any(target_os = "windows", target_os = "macos"))]
mod native;
#[cfg(target_os = "linux")]
mod x11;

use std::io::Cursor;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine as _;
use serde_json::Value;

use mermaid_domain::{ToolDefinition, ToolOutcome};
use mermaid_model::models::adapters::computer_toolset::MEMBERS;

use self::desktop::{Button, Desktop, Key, parse_chord};
use super::super::ctx::ExecContext;
use super::ToolExecutor;
use super::policy_gate::gate_computer;

/// The largest picture sent: the long edge and the pixel count every
/// current vision model accepts without the provider shrinking or refusing
/// it (Claude's older limit, 1568 px and 1.15 megapixels).
const MAX_EDGE: u32 = 1568;
const MAX_PIXELS: u64 = 1_150_000;

/// How long the screen gets to change after an input action before the
/// result returns (Anthropic's reference implementation waits the same).
const SETTLE: Duration = Duration::from_millis(500);

/// The longest `wait` or `hold_key`.
const MAX_SECS: f64 = 100.0;

/// Scroll clicks when the model names no `scroll_amount`.
const DEFAULT_SCROLL: i32 = 3;

/// Whether the tool can drive a screen here, or why not.
pub fn availability() -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        x11::availability()
    }
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    {
        Ok(())
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        Err("computer is not supported on this operating system".to_string())
    }
}

fn open_desktop() -> Result<Box<dyn Desktop>, String> {
    #[cfg(target_os = "linux")]
    {
        x11::X11::connect().map(|d| Box::new(d) as Box<dyn Desktop>)
    }
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    {
        native::Native::connect().map(|d| Box::new(d) as Box<dyn Desktop>)
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        Err("computer is not supported on this operating system".to_string())
    }
}

/// The size a `w` by `h` picture is sent at: as large as fits both limits,
/// never larger than it is.
fn fit(w: u32, h: u32) -> (u32, u32) {
    let pixels = u64::from(w) * u64::from(h);
    let scale = (f64::from(MAX_EDGE) / f64::from(w.max(h)))
        .min((MAX_PIXELS as f64 / pixels as f64).sqrt())
        .min(1.0);
    (
        ((f64::from(w) * scale).floor() as u32).max(1),
        ((f64::from(h) * scale).floor() as u32).max(1),
    )
}

/// The screen in the model's coordinates: pixels of the screenshot it was
/// sent, which is the capture fitted to the limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Shot {
    capture: (u32, u32),
    sent: (u32, u32),
}

impl Shot {
    fn of(capture: (u32, u32)) -> Self {
        Self {
            capture,
            sent: fit(capture.0, capture.1),
        }
    }

    fn check(&self, (x, y): (i64, i64)) -> Result<(), String> {
        let (w, h) = self.sent;
        if x < 0 || y < 0 || x >= i64::from(w) || y >= i64::from(h) {
            return Err(format!(
                "coordinate ({x}, {y}) is off the screen, which is {w}x{h} in screenshot pixels"
            ));
        }
        Ok(())
    }

    /// A screenshot point in `size` coordinates.
    fn scale_to(&self, (x, y): (i64, i64), size: (u32, u32)) -> (i32, i32) {
        let sx = f64::from(size.0) / f64::from(self.sent.0);
        let sy = f64::from(size.1) / f64::from(self.sent.1);
        (
            (x as f64 * sx).round() as i32,
            (y as f64 * sy).round() as i32,
        )
    }

    /// A `size` point in screenshot coordinates.
    fn scale_from(&self, (x, y): (i32, i32), size: (u32, u32)) -> (i64, i64) {
        let sx = f64::from(self.sent.0) / f64::from(size.0);
        let sy = f64::from(self.sent.1) / f64::from(size.1);
        (
            (f64::from(x) * sx).round() as i64,
            (f64::from(y) * sy).round() as i64,
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Action {
    Screenshot,
    Zoom([i64; 4]),
    CursorPosition,
    Wait(f64),
    Click {
        button: Button,
        count: u32,
        at: Option<(i64, i64)>,
        hold: Vec<Key>,
    },
    Drag {
        from: (i64, i64),
        to: (i64, i64),
        hold: Vec<Key>,
    },
    Move((i64, i64)),
    MouseDown(Option<(i64, i64)>),
    MouseUp(Option<(i64, i64)>),
    Scroll {
        at: Option<(i64, i64)>,
        dx: i32,
        dy: i32,
        hold: Vec<Key>,
    },
    Type(String),
    Key {
        chord: Vec<Key>,
        repeat: u32,
    },
    HoldKey {
        chord: Vec<Key>,
        secs: f64,
    },
    /// Actions run in order, then a screenshot. One that does not parse
    /// stops the batch where it stands. With `scale`, coordinates are
    /// fractions of the screen out of `scale` (Gemini's 0 to 999) rather
    /// than screenshot pixels.
    Batch {
        actions: Vec<Result<Action, String>>,
        scale: Option<u32>,
    },
}

impl Action {
    /// The action with every point read as a fraction of the screen out of
    /// `scale` and turned into pixels of a `sent` screenshot.
    fn scaled(self, scale: u32, sent: (u32, u32)) -> Self {
        let px = |(x, y): (i64, i64)| {
            let of = |v: i64, size: u32| {
                (v as f64 * f64::from(size) / f64::from(scale))
                    .round()
                    .clamp(0.0, f64::from(size.saturating_sub(1))) as i64
            };
            (of(x, sent.0), of(y, sent.1))
        };
        match self {
            Self::Zoom([x0, y0, x1, y1]) => {
                let (a, b) = px((x0, y0));
                let (c, d) = px((x1, y1));
                Self::Zoom([a, b, c, d])
            },
            Self::Click {
                button,
                count,
                at,
                hold,
            } => Self::Click {
                button,
                count,
                at: at.map(px),
                hold,
            },
            Self::Drag { from, to, hold } => Self::Drag {
                from: px(from),
                to: px(to),
                hold,
            },
            Self::Move(at) => Self::Move(px(at)),
            Self::MouseDown(at) => Self::MouseDown(at.map(px)),
            Self::MouseUp(at) => Self::MouseUp(at.map(px)),
            Self::Scroll { at, dx, dy, hold } => Self::Scroll {
                at: at.map(px),
                dx,
                dy,
                hold,
            },
            other => other,
        }
    }

    /// Whether the action changes anything on the screen.
    fn is_input(&self) -> bool {
        match self {
            Self::Screenshot | Self::Zoom(_) | Self::CursorPosition | Self::Wait(_) => false,
            Self::Batch { actions, .. } => actions.iter().flatten().any(Self::is_input),
            _ => true,
        }
    }
}

fn point(args: &Value, field: &str) -> Result<Option<(i64, i64)>, String> {
    match args.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(xy)) if xy.len() == 2 => match (xy[0].as_f64(), xy[1].as_f64()) {
            (Some(x), Some(y)) => Ok(Some((x.round() as i64, y.round() as i64))),
            _ => Err(format!("`{field}` must be two numbers, [x, y]")),
        },
        Some(_) => Err(format!("`{field}` must be two numbers, [x, y]")),
    }
}

fn required_point(args: &Value, field: &str, action: &str) -> Result<(i64, i64), String> {
    point(args, field)?.ok_or_else(|| format!("`{action}` needs `{field}`, [x, y]"))
}

fn text<'a>(args: &'a Value, action: &str) -> Result<&'a str, String> {
    args.get("text")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| format!("`{action}` needs `text`"))
}

/// Keys held during a click or scroll: `text` such as `shift` or `ctrl+alt`.
fn held(args: &Value) -> Result<Vec<Key>, String> {
    match args.get("text").and_then(Value::as_str) {
        Some(chord) if !chord.trim().is_empty() => parse_chord(chord),
        _ => Ok(Vec::new()),
    }
}

fn secs(args: &Value, action: &str) -> Result<f64, String> {
    let secs = args
        .get("duration")
        .and_then(Value::as_f64)
        .ok_or_else(|| format!("`{action}` needs `duration`, in seconds"))?;
    if !(0.0..=MAX_SECS).contains(&secs) {
        return Err(format!("`duration` must be from 0 to {MAX_SECS} seconds"));
    }
    Ok(secs)
}

fn parse(args: &Value) -> Result<Action, String> {
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .ok_or("`action` is required")?;
    let click = |button, count| -> Result<Action, String> {
        Ok(Action::Click {
            button,
            count,
            at: point(args, "coordinate")?,
            hold: held(args)?,
        })
    };
    Ok(match action {
        "screenshot" => Action::Screenshot,
        "zoom" => {
            let region = args
                .get("region")
                .and_then(Value::as_array)
                .filter(|r| r.len() == 4)
                .and_then(|r| r.iter().map(Value::as_f64).collect::<Option<Vec<_>>>())
                .ok_or("`zoom` needs `region`, [x0, y0, x1, y1]")?;
            let r: Vec<i64> = region.iter().map(|v| v.round() as i64).collect();
            Action::Zoom([r[0], r[1], r[2], r[3]])
        },
        "cursor_position" => Action::CursorPosition,
        "wait" => Action::Wait(secs(args, action)?),
        "left_click" => click(Button::Left, 1)?,
        "right_click" => click(Button::Right, 1)?,
        "middle_click" => click(Button::Middle, 1)?,
        "double_click" => click(Button::Left, 2)?,
        "triple_click" => click(Button::Left, 3)?,
        "left_click_drag" => Action::Drag {
            from: required_point(args, "start_coordinate", action)?,
            to: required_point(args, "coordinate", action)?,
            hold: held(args)?,
        },
        "mouse_move" => Action::Move(required_point(args, "coordinate", action)?),
        "left_mouse_down" => Action::MouseDown(point(args, "coordinate")?),
        "left_mouse_up" => Action::MouseUp(point(args, "coordinate")?),
        "scroll" => {
            let amount = args
                .get("scroll_amount")
                .and_then(Value::as_u64)
                .map_or(DEFAULT_SCROLL, |n| n.min(100) as i32);
            let (dx, dy) = match args.get("scroll_direction").and_then(Value::as_str) {
                Some("up") => (0, -amount),
                Some("down") => (0, amount),
                Some("left") => (-amount, 0),
                Some("right") => (amount, 0),
                _ => {
                    return Err("`scroll` needs `scroll_direction`: up, down, left or right".into());
                },
            };
            Action::Scroll {
                at: point(args, "coordinate")?,
                dx,
                dy,
                hold: held(args)?,
            }
        },
        "type" => Action::Type(text(args, action)?.to_string()),
        "key" => Action::Key {
            chord: parse_chord(text(args, action)?)?,
            repeat: args
                .get("repeat")
                .and_then(Value::as_u64)
                .map_or(1, |n| n.clamp(1, 100) as u32),
        },
        "hold_key" => Action::HoldKey {
            chord: parse_chord(text(args, action)?)?,
            secs: secs(args, action)?,
        },
        "batch" => batch(args)?,
        other => {
            return Err(format!(
                "unknown action \"{other}\"; the actions are {}",
                MEMBERS.join(", ")
            ));
        },
    })
}

/// A `batch`: its actions, or, when the provider refused the call
/// (`refused`), only that refusal, which stops it before anything runs.
fn batch(args: &Value) -> Result<Action, String> {
    let actions = match args.get("refused").and_then(Value::as_str) {
        Some(reason) => vec![Err(format!(
            "Not executed: the model's provider blocked this action: {reason}"
        ))],
        None => args
            .get("actions")
            .and_then(Value::as_array)
            .ok_or("`batch` needs `actions`")?
            .iter()
            .map(|one| match one.get("action").and_then(Value::as_str) {
                Some("batch") => Err("a batch cannot hold a batch".to_string()),
                _ => parse(one),
            })
            .collect(),
    };
    let scale = args
        .get("scale")
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .filter(|&n| n > 0);
    Ok(Action::Batch { actions, scale })
}

/// What a finished action returns: text, and a PNG for the looking actions
/// and a batch. A batch that stopped at a failure still returns the screen,
/// so it reports the failure as `failed` rather than as an error.
#[derive(Debug)]
struct Done {
    text: String,
    png: Option<Vec<u8>>,
    failed: bool,
}

impl Done {
    fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            png: None,
            failed: false,
        }
    }
}

fn png(picture: &image::RgbaImage, size: (u32, u32)) -> Result<Vec<u8>, String> {
    let resized;
    let picture = if picture.dimensions() == size {
        picture
    } else {
        resized = image::imageops::resize(
            picture,
            size.0,
            size.1,
            image::imageops::FilterType::Triangle,
        );
        &resized
    };
    let rgb = image::DynamicImage::ImageRgba8(picture.clone()).into_rgb8();
    let mut bytes = Vec::new();
    rgb.write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
        .map_err(|e| format!("could not encode the screenshot: {e}"))?;
    Ok(bytes)
}

/// Press `keys` in order.
fn press(desktop: &mut dyn Desktop, keys: &[Key]) -> Result<(), String> {
    keys.iter().try_for_each(|&k| desktop.key(k, true))
}

/// Release `keys` in reverse order, all of them even after a failure.
fn release(desktop: &mut dyn Desktop, keys: &[Key]) -> Result<(), String> {
    let mut result = Ok(());
    for &k in keys.iter().rev() {
        if let Err(e) = desktop.key(k, false) {
            result = result.and(Err(e));
        }
    }
    result
}

/// Run `body` with `keys` held, and release them whatever happens.
fn holding(
    desktop: &mut dyn Desktop,
    keys: &[Key],
    body: impl FnOnce(&mut dyn Desktop) -> Result<(), String>,
) -> Result<(), String> {
    let pressed = press(desktop, keys);
    let ran = pressed.and_then(|()| body(desktop));
    let released = release(desktop, keys);
    ran.and(released)
}

/// The screen as the model last saw it, capturing it if it has not looked yet.
fn current(desktop: &mut dyn Desktop, shot: &mut Option<Shot>) -> Result<Shot, String> {
    if let Some(s) = shot {
        return Ok(*s);
    }
    let s = Shot::of(desktop.capture()?.dimensions());
    *shot = Some(s);
    Ok(s)
}

/// A screenshot point in the input coordinates, refused when off the screen.
fn to_input(desktop: &mut dyn Desktop, s: &Shot, at: (i64, i64)) -> Result<(i32, i32), String> {
    s.check(at)?;
    Ok(s.scale_to(at, desktop.input_size()?))
}

/// Run one action on `desktop`. `shot` is the screen as the model last saw
/// it; a screenshot replaces it.
fn run(
    desktop: &mut dyn Desktop,
    shot: &mut Option<Shot>,
    action: &Action,
) -> Result<Done, String> {
    match action {
        Action::Screenshot | Action::Zoom(_) => look(desktop, shot, action),
        Action::Type(_) | Action::Key { .. } | Action::HoldKey { .. } | Action::Wait(_) => {
            keyboard(desktop, action)
        },
        _ => pointer(desktop, shot, action),
    }
}

/// `screenshot` and `zoom`.
fn look(
    desktop: &mut dyn Desktop,
    shot: &mut Option<Shot>,
    action: &Action,
) -> Result<Done, String> {
    match action {
        Action::Screenshot => {
            let picture = desktop.capture()?;
            let s = Shot::of(picture.dimensions());
            *shot = Some(s);
            Ok(Done {
                text: format!(
                    "Screenshot, {}x{} pixels. Coordinates are pixels of this picture.",
                    s.sent.0, s.sent.1
                ),
                png: Some(png(&picture, s.sent)?),
                failed: false,
            })
        },
        Action::Zoom([x0, y0, x1, y1]) => {
            let picture = desktop.capture()?;
            let s = Shot::of(picture.dimensions());
            *shot = Some(s);
            if x1 <= x0 || y1 <= y0 {
                return Err("`region` must be [x0, y0, x1, y1] with x1 > x0 and y1 > y0".into());
            }
            s.check((*x0, *y0))?;
            s.check((x1 - 1, y1 - 1))?;
            let (cx0, cy0) = s.scale_to((*x0, *y0), s.capture);
            let (cx1, cy1) = s.scale_to((*x1, *y1), s.capture);
            let (w, h) = ((cx1 - cx0).max(1) as u32, (cy1 - cy0).max(1) as u32);
            let crop = image::imageops::crop_imm(&picture, cx0 as u32, cy0 as u32, w, h).to_image();
            let size = fit(w, h);
            Ok(Done {
                text: format!(
                    "Zoom of [{x0}, {y0}, {x1}, {y1}], {}x{} pixels. Coordinates for other actions are still pixels of the last screenshot.",
                    size.0, size.1
                ),
                png: Some(png(&crop, size)?),
                failed: false,
            })
        },
        _ => unreachable!("not a looking action: {action:?}"),
    }
}

/// The mouse actions and `cursor_position`.
fn pointer(
    desktop: &mut dyn Desktop,
    shot: &mut Option<Shot>,
    action: &Action,
) -> Result<Done, String> {
    match action {
        Action::CursorPosition => {
            let s = current(desktop, shot)?;
            let input = desktop.input_size()?;
            let (x, y) = s.scale_from(desktop.cursor()?, input);
            Ok(Done::text(format!("X={x}, Y={y}")))
        },
        Action::Click {
            button,
            count,
            at,
            hold,
        } => {
            let s = current(desktop, shot)?;
            if let Some(at) = at {
                let (x, y) = to_input(desktop, &s, *at)?;
                desktop.move_to(x, y)?;
            }
            holding(desktop, hold, |d| {
                for _ in 0..*count {
                    d.button(*button, true)?;
                    d.button(*button, false)?;
                }
                Ok(())
            })?;
            let name = match (button, count) {
                (Button::Left, 2) => "double_click",
                (Button::Left, 3) => "triple_click",
                (Button::Left, _) => "left_click",
                (Button::Right, _) => "right_click",
                (Button::Middle, _) => "middle_click",
            };
            Ok(Done::text(match at {
                Some((x, y)) => format!("{name} at ({x}, {y})."),
                None => format!("{name} at the pointer."),
            }))
        },
        Action::Drag { from, to, hold } => {
            let s = current(desktop, shot)?;
            let start = to_input(desktop, &s, *from)?;
            let end = to_input(desktop, &s, *to)?;
            desktop.move_to(start.0, start.1)?;
            holding(desktop, hold, |d| {
                d.button(Button::Left, true)?;
                let moved = d.move_to(end.0, end.1);
                let released = d.button(Button::Left, false);
                moved.and(released)
            })?;
            Ok(Done::text(format!(
                "Dragged from ({}, {}) to ({}, {}).",
                from.0, from.1, to.0, to.1
            )))
        },
        Action::Move(at) => {
            let s = current(desktop, shot)?;
            let (x, y) = to_input(desktop, &s, *at)?;
            desktop.move_to(x, y)?;
            Ok(Done::text(format!(
                "Moved the pointer to ({}, {}).",
                at.0, at.1
            )))
        },
        Action::MouseDown(at) | Action::MouseUp(at) => {
            let down = matches!(action, Action::MouseDown(_));
            if let Some(at) = at {
                let s = current(desktop, shot)?;
                let (x, y) = to_input(desktop, &s, *at)?;
                desktop.move_to(x, y)?;
            }
            desktop.button(Button::Left, down)?;
            Ok(Done::text(if down {
                "Left button pressed."
            } else {
                "Left button released."
            }))
        },
        Action::Scroll { at, dx, dy, hold } => {
            let s = current(desktop, shot)?;
            if let Some(at) = at {
                let (x, y) = to_input(desktop, &s, *at)?;
                desktop.move_to(x, y)?;
            }
            holding(desktop, hold, |d| d.scroll(*dx, *dy))?;
            Ok(Done::text(format!(
                "Scrolled {} clicks.",
                dx.abs().max(dy.abs())
            )))
        },
        _ => unreachable!("not a mouse action: {action:?}"),
    }
}

/// The keyboard actions and `wait`.
fn keyboard(desktop: &mut dyn Desktop, action: &Action) -> Result<Done, String> {
    match action {
        Action::Wait(secs) => {
            std::thread::sleep(Duration::from_secs_f64(*secs));
            Ok(Done::text(format!("Waited {secs} s.")))
        },
        Action::Type(text) => {
            desktop.text(text)?;
            Ok(Done::text(format!(
                "Typed {} characters.",
                text.chars().count()
            )))
        },
        Action::Key { chord, repeat } => {
            for _ in 0..*repeat {
                holding(desktop, chord, |_| Ok(()))?;
            }
            Ok(Done::text(if *repeat > 1 {
                format!("Pressed the keys {repeat} times.")
            } else {
                "Pressed the keys.".to_string()
            }))
        },
        Action::HoldKey { chord, secs } => {
            holding(desktop, chord, |_| {
                std::thread::sleep(Duration::from_secs_f64(*secs));
                Ok(())
            })?;
            Ok(Done::text(format!("Held the keys for {secs} s.")))
        },
        _ => unreachable!("not a keyboard action: {action:?}"),
    }
}

/// What the approval prompt and the transcript show for an input action.
fn describe(args: &Value) -> String {
    let action = args.get("action").and_then(Value::as_str).unwrap_or("?");
    let mut parts = vec![format!("computer {action}")];
    for field in ["start_coordinate", "coordinate"] {
        if let Some(Value::Array(xy)) = args.get(field) {
            let xy: Vec<String> = xy.iter().map(ToString::to_string).collect();
            parts.push(format!("({})", xy.join(", ")));
        }
    }
    if let Some(dir) = args.get("scroll_direction").and_then(Value::as_str) {
        parts.push(dir.to_string());
    }
    if let Some(text) = args.get("text").and_then(Value::as_str) {
        let shown: String = text.chars().take(80).collect();
        let more = if text.chars().count() > 80 { "..." } else { "" };
        parts.push(format!("{shown:?}{more}"));
    }
    parts.join(" ")
}

/// Why an input action did not run after the user moved the real mouse.
const MOVED: &str = "Not executed: the user moved the mouse, so the user may be using the \
    screen. Mouse and keyboard actions stay stopped until the user sends a message.";

/// How far the pointer may sit from where Mermaid left it, in input
/// coordinates, before it counts as moved by the user.
const MOVE_TOLERANCE: i32 = 3;

/// One run of the agent: a session and the user messages in it so far. A new
/// user message starts a new run.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RunKey {
    session: Option<String>,
    messages: usize,
    latest: Option<String>,
}

impl RunKey {
    fn of(ctx: &ExecContext) -> Self {
        Self {
            session: ctx.session_id.clone(),
            messages: ctx.goal.requests.len() + ctx.goal.omitted,
            latest: ctx.goal.requests.last().cloned(),
        }
    }
}

/// Where Mermaid left the pointer after its last input action in a run.
#[derive(Debug, Clone)]
struct Watch {
    run: RunKey,
    pointer: Option<(i32, i32)>,
    /// The user moved the mouse in this run: input stays stopped.
    stopped: bool,
}

/// A batch the gate already allowed: the session, the turn, and the safety
/// mode it was allowed in.
type Grant = (
    Option<String>,
    mermaid_model::ids::TurnId,
    mermaid_runtime::SafetyMode,
);

#[derive(Debug, Default)]
struct ToolState {
    /// The size of the screen as the model last saw it.
    shot: Option<Shot>,
    /// The last full screenshot sent to the model, for the Auto-mode check.
    screen: Option<Vec<u8>>,
    watch: Option<Watch>,
    granted: Option<Grant>,
}

/// What the gate shows and decides for an input action: the action, or the
/// whole batch when the model sent several, in calls of their own or in one
/// `batch` call. Warnings the provider attached to a call come last.
fn gate_summary(args: &Value, batch: &[Value]) -> String {
    let calls = if batch.is_empty() {
        std::slice::from_ref(args)
    } else {
        batch
    };
    let actions: Vec<&Value> = calls
        .iter()
        .flat_map(|call| match call.get("actions").and_then(Value::as_array) {
            Some(inner) => inner.iter().collect(),
            None => vec![call],
        })
        .collect();
    let mut summary = match actions.as_slice() {
        [one] => describe(one),
        _ => {
            let described: Vec<String> = actions
                .iter()
                .map(|a| describe(a).trim_start_matches("computer ").to_string())
                .collect();
            format!(
                "computer, {} actions: {}",
                actions.len(),
                described.join("; ")
            )
        },
    };
    if calls.iter().any(|call| call.get("scale").is_some()) {
        summary.push_str(" (coordinates in thousandths of the screen)");
    }
    let warnings: Vec<&str> = calls
        .iter()
        .filter_map(|call| call.get("warnings").and_then(Value::as_array))
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    if warnings.is_empty() {
        summary
    } else {
        format!("{summary}. The provider warns: {}", warnings.join(" "))
    }
}

/// Run a batch's actions in order, each watched like a call of its own, and
/// stop at the first that fails. Then look: a batch always returns the
/// screen as its actions left it.
fn run_batch(
    desktop: &mut dyn Desktop,
    state: &mut ToolState,
    run_key: &RunKey,
    actions: &[Result<Action, String>],
    scale: Option<u32>,
) -> Done {
    let mut lines = Vec::new();
    let mut ran = 0;
    let mut changed = false;
    for action in actions {
        let done = action
            .clone()
            .and_then(|action| match scale {
                Some(scale) => {
                    let s = current(desktop, &mut state.shot)?;
                    Ok(action.scaled(scale, s.sent))
                },
                None => Ok(action),
            })
            .and_then(|action| run_watched(desktop, state, run_key, &action));
        match done {
            Ok(done) => {
                ran += 1;
                changed |= action.as_ref().is_ok_and(Action::is_input);
                lines.push(done.text);
            },
            Err(e) => {
                lines.push(format!("Error: {e}"));
                break;
            },
        }
    }
    let failed = ran < actions.len();
    if changed {
        std::thread::sleep(SETTLE);
    }
    let png = match run_watched(desktop, state, run_key, &Action::Screenshot) {
        Ok(shot) => {
            lines.push(shot.text);
            shot.png
        },
        Err(e) => {
            lines.push(format!("Could not take the screenshot: {e}"));
            None
        },
    };
    let noun = |n: usize| if n == 1 { "action" } else { "actions" };
    let head = if failed {
        format!(
            "Ran {ran} of {} {}; the rest did not run.",
            actions.len(),
            noun(actions.len())
        )
    } else {
        format!("Ran {ran} {}.", noun(ran))
    };
    lines.insert(0, head);
    Done {
        text: lines.join("\n"),
        failed: failed || png.is_none(),
        png,
    }
}

/// Run `action` with the pointer watch: an input action first checks that
/// the pointer is where Mermaid left it in this run, and after it runs,
/// records where the pointer is now.
fn run_watched(
    desktop: &mut dyn Desktop,
    state: &mut ToolState,
    run_key: &RunKey,
    action: &Action,
) -> Result<Done, String> {
    if let Action::Batch { actions, scale } = action {
        return Ok(run_batch(desktop, state, run_key, actions, *scale));
    }
    if !action.is_input() {
        let done = run(desktop, &mut state.shot, action);
        if let (Action::Screenshot, Ok(Done { png: Some(png), .. })) = (action, &done) {
            state.screen = Some(png.clone());
        }
        return done;
    }
    if let Some(watch) = state.watch.as_mut().filter(|w| w.run == *run_key) {
        let moved = watch
            .pointer
            .zip(desktop.cursor().ok())
            .is_some_and(|(was, now)| {
                (was.0 - now.0).abs() > MOVE_TOLERANCE || (was.1 - now.1).abs() > MOVE_TOLERANCE
            });
        if moved {
            watch.stopped = true;
            return Err(MOVED.to_string());
        }
    }
    let done = run(desktop, &mut state.shot, action);
    state.watch = Some(Watch {
        run: run_key.clone(),
        pointer: desktop.cursor().ok(),
        stopped: false,
    });
    done
}

/// The `computer` tool. It remembers the size of the last screenshot, so the
/// model's coordinates mean pixels of the picture it saw, and where it left
/// the pointer, so it stops when the user moves the mouse.
pub struct ComputerTool {
    state: Arc<Mutex<ToolState>>,
}

impl ComputerTool {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(ToolState::default())),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, ToolState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Run `action` on a screen opened by `open`, off the async runtime: the
    /// backends block. The blocking thread runs to the end even when the turn
    /// is cancelled, so a key held down is always released.
    async fn perform(
        &self,
        action: Action,
        run: RunKey,
        open: fn() -> Result<Box<dyn Desktop>, String>,
    ) -> Result<Done, String> {
        let state = Arc::clone(&self.state);
        tokio::task::spawn_blocking(move || {
            let mut desktop = open()?;
            let mut state = state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            run_watched(desktop.as_mut(), &mut state, &run, &action)
        })
        .await
        .map_err(|e| format!("the screen backend stopped: {e}"))?
    }

    /// Consult the policy gate for an input action, once per batch: a later
    /// action of a batch the gate allowed, in the same safety mode, runs
    /// without asking again.
    async fn gate(&self, args: &Value, ctx: &ExecContext) -> Option<ToolOutcome> {
        let grant: Grant = (ctx.session_id.clone(), ctx.turn, ctx.safety_mode);
        if self.state().granted.as_ref() == Some(&grant) {
            return None;
        }
        let summary = gate_summary(args, &ctx.computer_batch);
        let batch;
        let detail = if ctx.computer_batch.len() > 1 {
            batch = serde_json::json!({ "actions": ctx.computer_batch });
            &batch
        } else {
            args
        };
        let screen = self
            .state()
            .screen
            .as_ref()
            .map(|png| base64::engine::general_purpose::STANDARD.encode(png));
        let blocked = gate_computer(ctx, summary, detail, screen).await;
        if blocked.is_none() {
            self.state().granted = Some(grant);
        }
        blocked
    }

    /// A batch that did not run still returns the screen the model last saw:
    /// the provider that sends batches wants a picture back for every call.
    fn with_last_screen(&self, outcome: ToolOutcome, batch: bool) -> ToolOutcome {
        let screen = batch.then(|| self.state().screen.clone()).flatten();
        match screen {
            Some(png) => {
                outcome.with_images(vec![base64::engine::general_purpose::STANDARD.encode(png)])
            },
            None => outcome,
        }
    }

    async fn execute_with(
        &self,
        args: Value,
        ctx: &ExecContext,
        open: fn() -> Result<Box<dyn Desktop>, String>,
    ) -> ToolOutcome {
        let started = Instant::now();
        let action = match parse(&args) {
            Ok(action) => action,
            Err(e) => return ToolOutcome::error(e, None),
        };
        let input = action.is_input();
        let batch = matches!(action, Action::Batch { .. });
        let run = RunKey::of(ctx);
        if input {
            let stopped = self
                .state()
                .watch
                .as_ref()
                .is_some_and(|w| w.run == run && w.stopped);
            if stopped {
                return self.with_last_screen(ToolOutcome::error(MOVED, None), batch);
            }
            if let Some(blocked) = self.gate(&args, ctx).await {
                return self.with_last_screen(blocked, batch);
            }
        }
        let done = match action {
            // Waits on the runtime, so Esc ends it at once.
            Action::Wait(secs) => {
                tokio::time::sleep(Duration::from_secs_f64(secs)).await;
                Ok(Done::text(format!("Waited {secs} s.")))
            },
            action => self.perform(action, run, open).await,
        };
        let done = match done {
            Ok(done) => done,
            Err(e) => return ToolOutcome::error(e, Some(started.elapsed().as_secs_f64())),
        };
        // A batch settles before its closing screenshot.
        if input && !batch {
            tokio::time::sleep(SETTLE).await;
        }
        let elapsed = started.elapsed().as_secs_f64();
        let outcome = if done.failed {
            ToolOutcome::error(done.text, Some(elapsed))
        } else {
            let summary = done.text.lines().next().unwrap_or_default().to_string();
            ToolOutcome::success(done.text, summary, elapsed)
        };
        match done.png {
            Some(png) => {
                outcome.with_images(vec![base64::engine::general_purpose::STANDARD.encode(png)])
            },
            None => outcome,
        }
    }
}

impl Default for ComputerTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ToolExecutor for ComputerTool {
    fn name(&self) -> &'static str {
        "computer"
    }

    fn schema(&self) -> ToolDefinition {
        ToolDefinition {
            name: "computer".to_string(),
            description: "Use the user's real screen, mouse and keyboard. `screenshot` returns \
                a picture of the screen; coordinates in every other action are pixels of the \
                last screenshot. `zoom` returns `region` of the screen at a higher resolution. \
                Click actions take an optional `coordinate` and optional `text` naming keys to \
                hold, such as \"shift\". `key` and `hold_key` take `text` as a key or chord in \
                xdotool names, such as \"Return\" or \"ctrl+s\". `type` types `text`. Calls in \
                one message run in order, and after one fails the rest do not run."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": MEMBERS },
                    "coordinate": {
                        "type": "array", "items": { "type": "integer" },
                        "minItems": 2, "maxItems": 2,
                        "description": "[x, y] in screenshot pixels."
                    },
                    "start_coordinate": {
                        "type": "array", "items": { "type": "integer" },
                        "minItems": 2, "maxItems": 2,
                        "description": "`left_click_drag` only: where the drag starts."
                    },
                    "text": {
                        "type": "string",
                        "description": "Text for `type`; a key or chord for `key` and `hold_key`; keys to hold for clicks, `left_click_drag` and `scroll`."
                    },
                    "scroll_direction": { "type": "string", "enum": ["up", "down", "left", "right"] },
                    "scroll_amount": { "type": "integer", "minimum": 1, "description": "Wheel clicks. Default 3." },
                    "duration": { "type": "number", "minimum": 0, "maximum": MAX_SECS, "description": "Seconds, for `wait` and `hold_key`." },
                    "region": {
                        "type": "array", "items": { "type": "integer" },
                        "minItems": 4, "maxItems": 4,
                        "description": "`zoom` only: [x0, y0, x1, y1] in screenshot pixels."
                    },
                    "repeat": { "type": "integer", "minimum": 1, "description": "`key` only: how many times to press it." }
                },
                "required": ["action"]
            }),
        }
    }

    async fn execute(&self, args: Value, ctx: ExecContext) -> ToolOutcome {
        self.execute_with(args, &ctx, open_desktop).await
    }
}

#[cfg(test)]
mod tests;
