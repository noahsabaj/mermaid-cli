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
            // F13: wire mouse wheel scroll. `UI_MOUSE_SCROLL_LINES`
            // sets the delta per wheel tick to match the READMEs
            // "mouse wheel scrolls the chat" contract.
            CtMouseKind::ScrollUp => Some(Msg::MouseScroll {
                delta: crate::constants::UI_MOUSE_SCROLL_LINES as i16,
            }),
            CtMouseKind::ScrollDown => Some(Msg::MouseScroll {
                delta: -(crate::constants::UI_MOUSE_SCROLL_LINES as i16),
            }),
            _ => None,
        },
        CtEvent::Resize(w, h) => Some(Msg::Resize {
            width: w,
            height: h,
        }),
        CtEvent::FocusGained => Some(Msg::FocusChanged(true)),
        CtEvent::FocusLost => Some(Msg::FocusChanged(false)),
    }
}

/// Coalesce a burst of character/Enter key presses into a single paste.
///
/// crossterm 0.29 does not emit `Event::Paste` on the Windows console backend
/// (it only parses the bracketed-paste wrapper on Unix). There, a clipboard
/// paste arrives as a flood of individual `Char`/`Enter` key events; fed
/// one-by-one through the reducer that renders char-by-char and submits on
/// every embedded newline. This collapses such a burst into one `Msg::Paste`
/// so the text lands atomically with no spurious per-line submits — on every
/// platform.
///
/// `first` is the event the main loop already pulled. `drain` yields each
/// further *immediately-available* event and `None` once the input queue is
/// momentarily empty (the burst is over). Returns the primary `Msg` plus any
/// trailing events that were drained but aren't part of the burst and must be
/// processed separately.
///
/// A lone keystroke (no burst) returns a normal `Msg::Key`, so Enter still
/// submits and Ctrl+J still inserts a literal newline.
pub fn coalesce_key_burst(
    first: CtEvent,
    mut drain: impl FnMut() -> Option<CtEvent>,
) -> (Option<Msg>, Vec<Msg>) {
    // Only an unmodified character/Enter press can start a paste burst.
    // Anything else (arrows, Ctrl/Alt combos, mouse, resize) passes straight
    // through with no draining.
    let Some(first_char) = coalescible_char(&first) else {
        return (event_to_msg(first), Vec::new());
    };

    let mut buf = String::new();
    buf.push(first_char);
    let mut trailing = Vec::new();

    while let Some(evt) = drain() {
        // Skip key release/repeat without ending the burst.
        if let CtEvent::Key(k) = &evt
            && k.kind != KeyEventKind::Press
        {
            continue;
        }
        match coalescible_char(&evt) {
            Some(c) => buf.push(c),
            None => {
                // Not part of the paste — process it on its own next tick.
                if let Some(m) = event_to_msg(evt) {
                    trailing.push(m);
                }
                break;
            },
        }
    }

    if buf.chars().count() <= 1 {
        // Single keystroke, not a paste: keep normal key semantics.
        (event_to_msg(first), trailing)
    } else {
        (Some(Msg::Paste(Paste::Text(buf))), trailing)
    }
}

/// The character a key press contributes to a coalesced paste, or `None` when
/// the event isn't part of a paste burst. Only unmodified (no Ctrl/Alt)
/// `Char` and `Enter` presses qualify; Enter maps to a newline.
fn coalescible_char(event: &CtEvent) -> Option<char> {
    let CtEvent::Key(key) = event else {
        return None;
    };
    if key.kind != KeyEventKind::Press {
        return None;
    }
    if key.modifiers.intersects(CtMods::CONTROL | CtMods::ALT) {
        return None;
    }
    match key.code {
        CtKeyCode::Char(c) => Some(c),
        CtKeyCode::Enter => Some('\n'),
        // Pasted tabs arrive as Tab key events on the Windows console; fold
        // them into the burst so indented code survives a paste. A lone Tab
        // (no burst) still falls through to the normal key path below.
        CtKeyCode::Tab => Some('\t'),
        _ => None,
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
        Some("visible-reasoning") => SlashCmd::VisibleReasoning(arg),
        Some("safety") => match arg.as_deref() {
            None => SlashCmd::Safety(None),
            // Invalid value ⇒ `None` ⇒ the reducer shows current + options.
            Some(mode) => SlashCmd::Safety(crate::runtime::SafetyMode::parse(&mode.to_lowercase())),
        },
        Some("plan") => SlashCmd::Plan(arg),
        Some("config") => SlashCmd::Config,
        Some("clear") => SlashCmd::Clear,
        Some("save") => SlashCmd::Save(arg),
        Some("load") => SlashCmd::Load(arg),
        Some("list") => SlashCmd::List,
        Some("usage") => SlashCmd::Usage,
        Some("todos") => SlashCmd::Todos(arg),
        Some("context") => {
            use crate::domain::ContextCmd;
            let a = arg.as_deref().map(str::trim);
            SlashCmd::Context(match a {
                None | Some("") => ContextCmd::Show,
                Some("auto") => ContextCmd::Auto,
                Some("max") | Some("full") => ContextCmd::Max,
                Some(s) => {
                    if let Some(rest) = s.strip_prefix("offload") {
                        match rest.trim() {
                            "on" | "true" | "enable" | "yes" => ContextCmd::Offload(true),
                            "off" | "false" | "disable" | "no" | "" => ContextCmd::Offload(false),
                            // "offload garbage" → just show.
                            _ => ContextCmd::Show,
                        }
                    } else if let Ok(n) = s.parse::<u32>() {
                        ContextCmd::Set(n)
                    } else {
                        // Unrecognized arg → show (self-documenting report).
                        ContextCmd::Show
                    }
                },
            })
        },
        Some("compact") => SlashCmd::Compact(arg),
        Some("memory") => SlashCmd::Memory,
        Some("remember") => SlashCmd::Remember(arg),
        Some("forget") => SlashCmd::Forget(arg),
        Some("consolidate-memory") => SlashCmd::ConsolidateMemory,
        Some("doctor") => SlashCmd::Doctor,
        Some("tasks") => SlashCmd::Tasks,
        Some("task") => SlashCmd::Task(arg),
        Some("pause") => SlashCmd::Pause(arg),
        Some("resume") => SlashCmd::Resume(arg),
        Some("cancel") => SlashCmd::Cancel(arg),
        Some("handoff") => SlashCmd::Handoff(arg),
        Some("report") => SlashCmd::Report(arg),
        Some("agents") => SlashCmd::Agents(arg),
        Some("processes") => SlashCmd::Processes,
        Some("logs") => SlashCmd::Logs(arg),
        Some("stop") => SlashCmd::Stop(arg),
        Some("restart") => SlashCmd::Restart(arg),
        Some("open") => SlashCmd::Open(arg),
        Some("ports") => SlashCmd::Ports,
        Some("approvals") => SlashCmd::Approvals,
        Some("approve") => SlashCmd::Approve(arg),
        Some("deny") => SlashCmd::Deny(arg),
        Some("checkpoint") => SlashCmd::Checkpoint(arg),
        Some("checkpoints") => SlashCmd::Checkpoints,
        Some("restore") => SlashCmd::Restore(arg),
        Some("plugins") => SlashCmd::Plugins,
        Some("model-info") => SlashCmd::ModelInfo(arg),
        Some("cloud-setup") => SlashCmd::CloudSetup,
        Some("theme") => SlashCmd::Theme(arg),
        Some("editor") => SlashCmd::Editor,
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
    fn parses_theme_and_editor_commands() {
        assert_eq!(parse_slash_command("theme"), SlashCmd::Theme(None));
        assert_eq!(
            parse_slash_command("theme light"),
            SlashCmd::Theme(Some("light".to_string()))
        );
        assert_eq!(parse_slash_command("editor"), SlashCmd::Editor);
    }

    #[test]
    fn parses_agents_command_with_kill_tail() {
        assert_eq!(parse_slash_command("agents"), SlashCmd::Agents(None));
        assert_eq!(
            parse_slash_command("agents kill a1"),
            SlashCmd::Agents(Some("kill a1".to_string()))
        );
        assert_eq!(
            parse_slash_command("agents kill all"),
            SlashCmd::Agents(Some("kill all".to_string()))
        );
    }

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
                assert!(!k.modifiers.shift);
            },
            _ => panic!("wrong variant"),
        }
    }

    /// With the kitty keyboard protocol negotiated, Ctrl+Shift+C arrives as a
    /// distinct event; the SHIFT bit must survive translation so the reducer
    /// can tell the copy chord apart from the quit chord.
    #[test]
    fn translates_ctrl_shift_c_with_shift_intact() {
        let ev = CtEvent::Key(crossterm::event::KeyEvent {
            code: CtKeyCode::Char('c'),
            modifiers: CtMods::CONTROL | CtMods::SHIFT,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        });
        let msg = event_to_msg(ev).expect("msg");
        match msg {
            Msg::Key(k) => {
                assert_eq!(k.code, KeyCode::Char('c'));
                assert!(k.modifiers.ctrl);
                assert!(k.modifiers.shift);
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

    fn key(code: CtKeyCode) -> CtEvent {
        CtEvent::Key(crossterm::event::KeyEvent {
            code,
            modifiers: CtMods::NONE,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        })
    }

    fn key_with(code: CtKeyCode, modifiers: CtMods, kind: KeyEventKind) -> CtEvent {
        CtEvent::Key(crossterm::event::KeyEvent {
            code,
            modifiers,
            kind,
            state: crossterm::event::KeyEventState::NONE,
        })
    }

    #[test]
    fn coalesce_single_char_stays_a_key() {
        let (primary, trailing) = coalesce_key_burst(key(CtKeyCode::Char('a')), || None);
        assert!(matches!(primary, Some(Msg::Key(k)) if k.code == KeyCode::Char('a')));
        assert!(trailing.is_empty());
    }

    #[test]
    fn coalesce_lone_enter_still_submits_as_key() {
        // A deliberate Enter (send) must NOT be turned into a paste.
        let (primary, _) = coalesce_key_burst(key(CtKeyCode::Enter), || None);
        assert!(
            matches!(primary, Some(Msg::Key(k)) if k.code == KeyCode::Enter),
            "a lone Enter must remain a key, not become a paste"
        );
    }

    #[test]
    fn coalesce_burst_of_chars_becomes_one_paste() {
        let mut rest = vec![key(CtKeyCode::Char('e')), key(CtKeyCode::Char('y'))].into_iter();
        let (primary, trailing) = coalesce_key_burst(key(CtKeyCode::Char('h')), || rest.next());
        match primary {
            Some(Msg::Paste(Paste::Text(s))) => assert_eq!(s, "hey"),
            other => panic!("expected paste, got {other:?}"),
        }
        assert!(trailing.is_empty());
    }

    #[test]
    fn coalesce_preserves_pasted_newlines_without_submitting() {
        // The reported bug: each Enter in a paste submitted a line. The
        // burst must collapse to one multi-line paste instead.
        let mut rest = vec![key(CtKeyCode::Enter), key(CtKeyCode::Char('b'))].into_iter();
        let (primary, _) = coalesce_key_burst(key(CtKeyCode::Char('a')), || rest.next());
        match primary {
            Some(Msg::Paste(Paste::Text(s))) => assert_eq!(s, "a\nb"),
            other => panic!("expected multi-line paste, got {other:?}"),
        }
    }

    #[test]
    fn coalesce_stops_at_non_char_and_enqueues_it() {
        let mut rest = vec![key(CtKeyCode::Char('b')), key(CtKeyCode::Esc)].into_iter();
        let (primary, trailing) = coalesce_key_burst(key(CtKeyCode::Char('a')), || rest.next());
        assert!(matches!(primary, Some(Msg::Paste(Paste::Text(ref s))) if s == "ab"));
        assert_eq!(trailing.len(), 1);
        assert!(matches!(trailing[0], Msg::Key(k) if k.code == KeyCode::Escape));
    }

    #[test]
    fn coalesce_skips_release_events_mid_burst() {
        let mut rest = vec![
            key_with(CtKeyCode::Char('x'), CtMods::NONE, KeyEventKind::Release),
            key(CtKeyCode::Char('b')),
        ]
        .into_iter();
        let (primary, _) = coalesce_key_burst(key(CtKeyCode::Char('a')), || rest.next());
        assert!(
            matches!(primary, Some(Msg::Paste(Paste::Text(ref s))) if s == "ab"),
            "release events must be skipped, not appended or treated as burst-enders"
        );
    }

    #[test]
    fn coalesce_preserves_pasted_tabs() {
        let mut rest = vec![key(CtKeyCode::Tab), key(CtKeyCode::Char('b'))].into_iter();
        let (primary, _) = coalesce_key_burst(key(CtKeyCode::Char('a')), || rest.next());
        match primary {
            Some(Msg::Paste(Paste::Text(s))) => assert_eq!(s, "a\tb"),
            other => panic!("expected paste with tab, got {other:?}"),
        }
    }

    #[test]
    fn coalesce_lone_tab_stays_a_key() {
        // A single Tab (palette completion etc.) must not become a paste.
        let (primary, _) = coalesce_key_burst(key(CtKeyCode::Tab), || None);
        assert!(matches!(primary, Some(Msg::Key(k)) if k.code == KeyCode::Tab));
    }

    #[test]
    fn coalesce_ctrl_combo_passes_through_without_draining() {
        let drained = std::cell::Cell::new(false);
        let (primary, trailing) = coalesce_key_burst(
            key_with(CtKeyCode::Char('c'), CtMods::CONTROL, KeyEventKind::Press),
            || {
                drained.set(true);
                None
            },
        );
        assert!(
            !drained.get(),
            "a non-coalescible first event must not drain the queue"
        );
        assert!(
            matches!(primary, Some(Msg::Key(k)) if k.code == KeyCode::Char('c') && k.modifiers.ctrl)
        );
        assert!(trailing.is_empty());
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
    fn parse_slash_usage_and_context() {
        use crate::domain::ContextCmd;
        assert_eq!(parse_slash_command("usage"), SlashCmd::Usage);
        assert_eq!(
            parse_slash_command("context"),
            SlashCmd::Context(ContextCmd::Show)
        );
        assert_eq!(
            parse_slash_command("context 65536"),
            SlashCmd::Context(ContextCmd::Set(65536))
        );
        assert_eq!(
            parse_slash_command("context auto"),
            SlashCmd::Context(ContextCmd::Auto)
        );
        assert_eq!(
            parse_slash_command("context max"),
            SlashCmd::Context(ContextCmd::Max)
        );
        assert_eq!(
            parse_slash_command("context offload on"),
            SlashCmd::Context(ContextCmd::Offload(true))
        );
        assert_eq!(
            parse_slash_command("context offload off"),
            SlashCmd::Context(ContextCmd::Offload(false))
        );
        // Unrecognized arg falls back to the (self-documenting) report.
        assert_eq!(
            parse_slash_command("context wat"),
            SlashCmd::Context(ContextCmd::Show)
        );
        assert_eq!(parse_slash_command("doctor"), SlashCmd::Doctor);
    }

    #[test]
    fn parse_slash_compact_and_aliases() {
        assert_eq!(parse_slash_command("compact"), SlashCmd::Compact(None));
        assert_eq!(
            parse_slash_command("compact focus on tests"),
            SlashCmd::Compact(Some("focus on tests".to_string()))
        );
        assert_eq!(parse_slash_command("compress"), SlashCmd::Compact(None));
        assert_eq!(parse_slash_command("summarize"), SlashCmd::Compact(None));
    }

    #[test]
    fn parse_memory_commands() {
        assert_eq!(parse_slash_command("memory"), SlashCmd::Memory);
        assert_eq!(parse_slash_command("memories"), SlashCmd::Memory); // alias
        assert_eq!(
            parse_slash_command("remember prefer ripgrep"),
            SlashCmd::Remember(Some("prefer ripgrep".to_string()))
        );
        assert_eq!(parse_slash_command("remember"), SlashCmd::Remember(None));
        assert_eq!(
            parse_slash_command("forget prefer-ripgrep"),
            SlashCmd::Forget(Some("prefer-ripgrep".to_string()))
        );
        assert_eq!(parse_slash_command("forget"), SlashCmd::Forget(None));
        assert_eq!(
            parse_slash_command("consolidate-memory"),
            SlashCmd::ConsolidateMemory
        );
        assert_eq!(
            parse_slash_command("prune-memory"),
            SlashCmd::ConsolidateMemory
        ); // alias
    }

    #[test]
    fn parse_runtime_task_commands() {
        assert_eq!(parse_slash_command("tasks"), SlashCmd::Tasks);
        assert_eq!(
            parse_slash_command("task task-123"),
            SlashCmd::Task(Some("task-123".to_string()))
        );
        assert_eq!(
            parse_slash_command("pause task-123"),
            SlashCmd::Pause(Some("task-123".to_string()))
        );
        assert_eq!(
            parse_slash_command("resume task-123"),
            SlashCmd::Resume(Some("task-123".to_string()))
        );
        assert_eq!(parse_slash_command("cancel"), SlashCmd::Cancel(None));
        assert_eq!(
            parse_slash_command("handoff task-123"),
            SlashCmd::Handoff(Some("task-123".to_string()))
        );
        assert_eq!(parse_slash_command("report"), SlashCmd::Report(None));
        assert_eq!(parse_slash_command("procs"), SlashCmd::Processes);
        assert_eq!(parse_slash_command("approvals"), SlashCmd::Approvals);
        assert_eq!(
            parse_slash_command("approve approval-1"),
            SlashCmd::Approve(Some("approval-1".to_string()))
        );
        assert_eq!(
            parse_slash_command("deny approval-1"),
            SlashCmd::Deny(Some("approval-1".to_string()))
        );
        assert_eq!(
            parse_slash_command("checkpoint src/lib.rs"),
            SlashCmd::Checkpoint(Some("src/lib.rs".to_string()))
        );
        assert_eq!(parse_slash_command("checkpoints"), SlashCmd::Checkpoints);
        assert_eq!(
            parse_slash_command("restore checkpoint-1"),
            SlashCmd::Restore(Some("checkpoint-1".to_string()))
        );
        assert_eq!(parse_slash_command("plugins"), SlashCmd::Plugins);
    }

    #[test]
    fn parse_slash_reasoning_valid_level() {
        assert_eq!(
            parse_slash_command("reasoning high"),
            SlashCmd::Reasoning(Some(crate::models::ReasoningLevel::High)),
        );
    }

    #[test]
    fn parse_slash_visible_reasoning_and_alias() {
        assert_eq!(
            parse_slash_command("visible-reasoning on"),
            SlashCmd::VisibleReasoning(Some("on".to_string())),
        );
        assert_eq!(
            parse_slash_command("visiblereasoning"),
            SlashCmd::VisibleReasoning(None),
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
    fn parse_safety_command() {
        assert_eq!(
            parse_slash_command("safety auto"),
            SlashCmd::Safety(Some(crate::runtime::SafetyMode::Auto)),
        );
        // `/permission` is an alias that routes to the same command.
        assert_eq!(
            parse_slash_command("permission read_only"),
            SlashCmd::Safety(Some(crate::runtime::SafetyMode::ReadOnly)),
        );
        // No arg → show current; bogus value → None (show current + options).
        assert_eq!(parse_slash_command("safety"), SlashCmd::Safety(None));
        assert_eq!(parse_slash_command("safety bogus"), SlashCmd::Safety(None));
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
