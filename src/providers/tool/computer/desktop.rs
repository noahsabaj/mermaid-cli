//! The physics the `computer` tool needs from a screen: take a picture, move
//! and press the mouse, press keys, type text. One small trait, one backend
//! per platform, and a fake for tests.

/// A key the model can name: a character, or a named key from the X keysym
/// names Claude's computer tool is trained on (`Return`, `ctrl`, `Page_Down`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Key {
    Char(char),
    Return,
    Tab,
    Escape,
    Backspace,
    Delete,
    Insert,
    Home,
    End,
    PageUp,
    PageDown,
    Left,
    Up,
    Right,
    Down,
    Space,
    /// F1 to F20.
    F(u8),
    Shift,
    Control,
    Alt,
    Super,
    CapsLock,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Button {
    Left,
    Middle,
    Right,
}

/// One screen. Capture pixels and input coordinates can differ (a Retina
/// Mac captures at twice its point size), so each side reports its own size
/// and the tool scales between them.
pub(crate) trait Desktop {
    /// The whole primary screen, in capture pixels.
    fn capture(&mut self) -> Result<image::RgbaImage, String>;
    /// The primary screen's size in the coordinates `move_to` takes.
    fn input_size(&mut self) -> Result<(u32, u32), String>;
    fn move_to(&mut self, x: i32, y: i32) -> Result<(), String>;
    /// The pointer, in input coordinates.
    fn cursor(&mut self) -> Result<(i32, i32), String>;
    fn button(&mut self, button: Button, down: bool) -> Result<(), String>;
    /// Scroll by wheel clicks: positive `dy` is down, positive `dx` is right.
    fn scroll(&mut self, dx: i32, dy: i32) -> Result<(), String>;
    fn key(&mut self, key: Key, down: bool) -> Result<(), String>;
    fn text(&mut self, text: &str) -> Result<(), String>;
}

/// Parse one key name, case-insensitively: a single character, or a name
/// from the X keysym set with the common aliases (`enter`, `esc`, `cmd`).
pub(crate) fn parse_key(name: &str) -> Result<Key, String> {
    let mut chars = name.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        return Ok(if c == ' ' { Key::Space } else { Key::Char(c) });
    }
    let lower = name.to_ascii_lowercase();
    let key = match lower.as_str() {
        "return" | "enter" | "kp_enter" => Key::Return,
        "tab" => Key::Tab,
        "escape" | "esc" => Key::Escape,
        "backspace" | "back_space" => Key::Backspace,
        "delete" | "del" => Key::Delete,
        "insert" => Key::Insert,
        "home" => Key::Home,
        "end" => Key::End,
        "page_up" | "pageup" | "prior" => Key::PageUp,
        "page_down" | "pagedown" | "next" => Key::PageDown,
        "left" => Key::Left,
        "up" => Key::Up,
        "right" => Key::Right,
        "down" => Key::Down,
        "space" => Key::Space,
        "shift" | "shift_l" | "shift_r" => Key::Shift,
        "ctrl" | "control" | "control_l" | "control_r" => Key::Control,
        "alt" | "alt_l" | "alt_r" | "option" => Key::Alt,
        "super" | "super_l" | "super_r" | "cmd" | "command" | "meta" | "win" | "windows" => {
            Key::Super
        },
        "caps_lock" | "capslock" => Key::CapsLock,
        "minus" => Key::Char('-'),
        "plus" => Key::Char('+'),
        "equal" => Key::Char('='),
        "comma" => Key::Char(','),
        "period" => Key::Char('.'),
        "slash" => Key::Char('/'),
        "backslash" => Key::Char('\\'),
        "semicolon" => Key::Char(';'),
        "apostrophe" => Key::Char('\''),
        "grave" => Key::Char('`'),
        "bracketleft" => Key::Char('['),
        "bracketright" => Key::Char(']'),
        _ => match lower.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
            Some(n @ 1..=20) => Key::F(n),
            _ => return Err(format!("unknown key \"{name}\"")),
        },
    };
    Ok(key)
}

/// Parse a chord such as `ctrl+shift+t`: every key in it, in order. A
/// trailing `+` names the plus key (`ctrl++`).
pub(crate) fn parse_chord(chord: &str) -> Result<Vec<Key>, String> {
    let chord = chord.trim();
    if chord.is_empty() {
        return Err("no key given".to_string());
    }
    let (body, plus) = match chord.strip_suffix("++") {
        Some(body) => (body, true),
        None if chord == "+" => ("", true),
        None => (chord, false),
    };
    let mut keys = body
        .split('+')
        .filter(|part| !part.is_empty())
        .map(|part| parse_key(part.trim()))
        .collect::<Result<Vec<_>, _>>()?;
    if plus {
        keys.push(Key::Char('+'));
    }
    Ok(keys)
}

#[cfg(test)]
pub(crate) mod fake {
    //! A screen in memory that records what the tool did to it.

    use super::{Button, Desktop, Key};

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) enum Event {
        Move(i32, i32),
        Button(Button, bool),
        Scroll(i32, i32),
        Key(Key, bool),
        Text(String),
    }

    pub(crate) struct FakeDesktop {
        pub capture_size: (u32, u32),
        pub input_size: (u32, u32),
        pub cursor: (i32, i32),
        pub events: Vec<Event>,
    }

    impl FakeDesktop {
        pub(crate) fn new(capture_size: (u32, u32), input_size: (u32, u32)) -> Self {
            Self {
                capture_size,
                input_size,
                cursor: (0, 0),
                events: Vec::new(),
            }
        }
    }

    impl Desktop for FakeDesktop {
        fn capture(&mut self) -> Result<image::RgbaImage, String> {
            let (w, h) = self.capture_size;
            // A gradient, so a crop shows which part of the screen it took.
            Ok(image::RgbaImage::from_fn(w, h, |x, y| {
                image::Rgba([(x % 256) as u8, (y % 256) as u8, 0, 255])
            }))
        }
        fn input_size(&mut self) -> Result<(u32, u32), String> {
            Ok(self.input_size)
        }
        fn move_to(&mut self, x: i32, y: i32) -> Result<(), String> {
            self.cursor = (x, y);
            self.events.push(Event::Move(x, y));
            Ok(())
        }
        fn cursor(&mut self) -> Result<(i32, i32), String> {
            Ok(self.cursor)
        }
        fn button(&mut self, button: Button, down: bool) -> Result<(), String> {
            self.events.push(Event::Button(button, down));
            Ok(())
        }
        fn scroll(&mut self, dx: i32, dy: i32) -> Result<(), String> {
            self.events.push(Event::Scroll(dx, dy));
            Ok(())
        }
        fn key(&mut self, key: Key, down: bool) -> Result<(), String> {
            self.events.push(Event::Key(key, down));
            Ok(())
        }
        fn text(&mut self, text: &str) -> Result<(), String> {
            self.events.push(Event::Text(text.to_string()));
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chords_parse_in_the_names_the_model_writes() {
        assert_eq!(
            parse_chord("ctrl+shift+t").unwrap(),
            vec![Key::Control, Key::Shift, Key::Char('t')]
        );
        assert_eq!(parse_chord("Return").unwrap(), vec![Key::Return]);
        assert_eq!(parse_chord("alt+F4").unwrap(), vec![Key::Alt, Key::F(4)]);
        assert_eq!(
            parse_chord("cmd+Page_Down").unwrap(),
            vec![Key::Super, Key::PageDown]
        );
        assert_eq!(
            parse_chord("ctrl++").unwrap(),
            vec![Key::Control, Key::Char('+')]
        );
        assert_eq!(
            parse_chord("ctrl+minus").unwrap(),
            vec![Key::Control, Key::Char('-')]
        );
        assert_eq!(parse_chord(" ").unwrap_err(), "no key given");
        assert_eq!(
            parse_chord("ctrl+hyper").unwrap_err(),
            "unknown key \"hyper\""
        );
        assert!(parse_chord("F21").is_err());
    }
}
