//! Crossterm event stream → `Msg`.
//!
//! One of two branches in the main loop's central `select!`.
//! Crossterm's `EventStream` yields key presses, mouse events,
//! pastes, and resize notifications; we translate each into the
//! typed `Msg` vocabulary the reducer understands.
//!
//! The event source knows nothing about state. The reducer owns
//! the transitions; the event source just produces typed inputs.
//!
//! For `--replay`, a second event source (in `recorder.rs`) reads
//! previously-recorded JSONL and yields the same Msg stream. The
//! main loop can't tell live crossterm events apart from replayed
//! ones — that's the point.

use crossterm::event::{
    Event as CtEvent, KeyCode as CtKeyCode, KeyEventKind, KeyModifiers as CtMods,
    MouseEventKind as CtMouseKind,
};

use crate::domain::{Key, KeyCode, KeyMods, Msg, Paste};

/// Translate one crossterm event into `Msg`. Returns `None` for
/// events the reducer doesn't care about (focus gained/lost, unknown
/// media keys, key repeats, etc.).
pub fn event_to_msg(event: CtEvent) -> Option<Msg> {
    match event {
        CtEvent::Key(key) => {
            // Skip KeyEventKind::Release and ::Repeat — we only act on
            // initial press. Release events fire twice as many Keys
            // and bloat any recorded session.
            if key.kind != KeyEventKind::Press {
                return None;
            }
            Some(Msg::Key(Key {
                code: translate_key_code(key.code)?,
                modifiers: translate_mods(key.modifiers),
            }))
        },
        CtEvent::Paste(text) => {
            if text.is_empty() {
                None
            } else {
                Some(Msg::Paste(Paste::Text(text)))
            }
        },
        CtEvent::Mouse(mouse) => match mouse.kind {
            CtMouseKind::ScrollUp | CtMouseKind::ScrollDown => {
                // Scroll bindings are rendered-only; the reducer
                // doesn't need mouse state. Map to a sentinel Key
                // so the UI layer can still update chat_scroll —
                // actually for now, just drop. Mouse support will
                // re-land in C7 when the render layer ports.
                None
            },
            _ => None,
        },
        CtEvent::Resize(w, h) => Some(Msg::Resize {
            width: w,
            height: h,
        }),
        CtEvent::FocusGained | CtEvent::FocusLost => None,
    }
}

fn translate_key_code(code: CtKeyCode) -> Option<KeyCode> {
    Some(match code {
        CtKeyCode::Char(c) => KeyCode::Char(c),
        CtKeyCode::Enter => KeyCode::Enter,
        CtKeyCode::Esc => KeyCode::Escape,
        CtKeyCode::Backspace => KeyCode::Backspace,
        CtKeyCode::Delete => KeyCode::Delete,
        CtKeyCode::Tab => KeyCode::Tab,
        CtKeyCode::BackTab => KeyCode::BackTab,
        CtKeyCode::Left => KeyCode::Left,
        CtKeyCode::Right => KeyCode::Right,
        CtKeyCode::Up => KeyCode::Up,
        CtKeyCode::Down => KeyCode::Down,
        CtKeyCode::Home => KeyCode::Home,
        CtKeyCode::End => KeyCode::End,
        CtKeyCode::PageUp => KeyCode::PageUp,
        CtKeyCode::PageDown => KeyCode::PageDown,
        CtKeyCode::F(n) => KeyCode::F(n),
        _ => return Some(KeyCode::Unknown),
    })
}

fn translate_mods(mods: CtMods) -> KeyMods {
    KeyMods {
        ctrl: mods.contains(CtMods::CONTROL),
        alt: mods.contains(CtMods::ALT),
        shift: mods.contains(CtMods::SHIFT),
    }
}

/// Parse a slash-command input line (without the leading `/`) into a
/// `SlashCmd`. Returns `SlashCmd::Unknown` if the command isn't in
/// the registry. Shared between the TUI dispatcher (C8) and any
/// non-interactive command dispatch.
pub fn parse_slash_command(raw: &str) -> crate::domain::SlashCmd {
    use crate::domain::SlashCmd;
    let trimmed = raw.trim();
    let (name, arg) = match trimmed.split_once(' ') {
        Some((n, a)) => (n.to_lowercase(), Some(a.trim().to_string())),
        None => (trimmed.to_lowercase(), None),
    };

    // Route through the registry so command aliases (/q → /quit) work.
    use crate::domain::slash_commands::COMMAND_REGISTRY;
    let canonical = COMMAND_REGISTRY
        .iter()
        .find(|c| c.name == name.as_str() || c.aliases.contains(&name.as_str()))
        .map(|c| c.name);

    match canonical {
        Some("model") => SlashCmd::Model(arg),
        Some("reasoning") => match arg.as_deref() {
            None => SlashCmd::Reasoning(None),
            Some(level) => {
                use clap::ValueEnum;
                SlashCmd::Reasoning(
                    crate::models::ReasoningLevel::from_str(&level.to_lowercase(), true).ok(),
                )
            },
        },
        Some("clear") => SlashCmd::Clear,
        Some("save") => SlashCmd::Save(arg),
        Some("load") => SlashCmd::Load(arg),
        Some("list") => SlashCmd::List,
        Some("cloud-setup") => SlashCmd::CloudSetup,
        Some("help") => SlashCmd::Help,
        Some("quit") => SlashCmd::Quit,
        _ => SlashCmd::Unknown(name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::SlashCmd;

    #[test]
    fn translates_printable_char_key() {
        let ev = CtEvent::Key(crossterm::event::KeyEvent {
            code: CtKeyCode::Char('a'),
            modifiers: CtMods::NONE,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        });
        let msg = event_to_msg(ev).expect("msg");
        match msg {
            Msg::Key(k) => {
                assert_eq!(k.code, KeyCode::Char('a'));
                assert!(k.modifiers.is_empty());
            },
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn translates_ctrl_c() {
        let ev = CtEvent::Key(crossterm::event::KeyEvent {
            code: CtKeyCode::Char('c'),
            modifiers: CtMods::CONTROL,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        });
        let msg = event_to_msg(ev).expect("msg");
        match msg {
            Msg::Key(k) => {
                assert_eq!(k.code, KeyCode::Char('c'));
                assert!(k.modifiers.ctrl);
                assert!(!k.modifiers.alt);
            },
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn skips_release_events() {
        let ev = CtEvent::Key(crossterm::event::KeyEvent {
            code: CtKeyCode::Char('a'),
            modifiers: CtMods::NONE,
            kind: KeyEventKind::Release,
            state: crossterm::event::KeyEventState::NONE,
        });
        assert!(event_to_msg(ev).is_none());
    }

    #[test]
    fn resize_translates_to_resize_msg() {
        let ev = CtEvent::Resize(80, 24);
        let msg = event_to_msg(ev).expect("msg");
        match msg {
            Msg::Resize { width, height } => {
                assert_eq!(width, 80);
                assert_eq!(height, 24);
            },
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn empty_paste_dropped() {
        let ev = CtEvent::Paste(String::new());
        assert!(event_to_msg(ev).is_none());
    }

    #[test]
    fn paste_translates_to_text_paste() {
        let ev = CtEvent::Paste("hello".to_string());
        let msg = event_to_msg(ev).expect("msg");
        match msg {
            Msg::Paste(Paste::Text(s)) => assert_eq!(s, "hello"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn parse_slash_model_no_arg() {
        assert_eq!(parse_slash_command("model"), SlashCmd::Model(None));
    }

    #[test]
    fn parse_slash_model_with_arg() {
        assert_eq!(
            parse_slash_command("model anthropic/opus"),
            SlashCmd::Model(Some("anthropic/opus".to_string())),
        );
    }

    #[test]
    fn parse_slash_quit_alias_q() {
        assert_eq!(parse_slash_command("q"), SlashCmd::Quit);
    }

    #[test]
    fn parse_slash_reasoning_valid_level() {
        assert_eq!(
            parse_slash_command("reasoning high"),
            SlashCmd::Reasoning(Some(crate::models::ReasoningLevel::High)),
        );
    }

    #[test]
    fn parse_slash_reasoning_invalid_level_is_none_arg() {
        // Argument exists but can't be parsed to a level — degrades
        // to showing current (None arg) rather than erroring.
        assert_eq!(
            parse_slash_command("reasoning bogus"),
            SlashCmd::Reasoning(None),
        );
    }

    #[test]
    fn parse_slash_unknown_command() {
        match parse_slash_command("nope") {
            SlashCmd::Unknown(name) => assert_eq!(name, "nope"),
            other => panic!("expected Unknown, got {:?}", other),
        }
    }

    #[test]
    fn key_mods_combine_correctly() {
        let mods = translate_mods(CtMods::CONTROL | CtMods::SHIFT);
        assert!(mods.ctrl);
        assert!(mods.shift);
        assert!(!mods.alt);
    }
}
