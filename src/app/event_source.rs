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

use mermaid_domain::{Key, KeyCode, KeyMods, Msg, Paste};

/// Translate one crossterm event into `Msg`. Returns `None` for
/// events the reducer doesn't care about (focus gained/lost, unknown
/// media keys, key repeats, etc.).
#[must_use]
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
                delta: mermaid_model::constants::UI_MOUSE_SCROLL_LINES as i16,
            }),
            CtMouseKind::ScrollDown => Some(Msg::MouseScroll {
                delta: -(mermaid_model::constants::UI_MOUSE_SCROLL_LINES as i16),
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
    drain: impl FnMut() -> Option<CtEvent>,
) -> (Option<Msg>, Vec<Msg>) {
    let (primary, trailing, _) = coalesce_key_burst_seamed(first, drain);
    (primary, trailing)
}

/// [`coalesce_key_burst`] plus the CRLF fold state at burst end, for the one
/// caller that bridges chunk gaps: a pair split at the seam — a CR closing
/// this burst, its LF opening the next chunk — must still fold to one
/// newline, and only this state can tell the bridge so.
pub(crate) fn coalesce_key_burst_seamed(
    first: CtEvent,
    drain: impl FnMut() -> Option<CtEvent>,
) -> (Option<Msg>, Vec<Msg>, bool) {
    // Only a coalescible press can start a paste burst. Anything else
    // (arrows, Ctrl/Alt chords, mouse, resize) passes straight through with
    // no draining.
    if coalescible_char(&first).is_none() {
        return (event_to_msg(first), Vec::new(), false);
    }

    let mut buf = String::new();
    let mut last_was_cr = false;
    let breaker = drain_burst_into(&mut buf, &mut last_was_cr, first.clone(), drain);
    let trailing: Vec<Msg> = breaker.and_then(event_to_msg).into_iter().collect();

    if buf.chars().count() <= 1 {
        // Single keystroke, not a paste: keep normal key semantics.
        (event_to_msg(first), trailing, last_was_cr)
    } else {
        (Some(Msg::Paste(Paste::Text(buf))), trailing, last_was_cr)
    }
}

/// Accumulate a burst into `buf`, starting from `first` (returned unfolded
/// if it is not coalescible). Returns the event that broke the burst, if
/// any — untranslated, for the caller to queue after the paste.
///
/// `last_was_cr` carries CRLF fold state in and out, so a caller stitching
/// bursts across chunk gaps deduplicates a pair split at the seam.
fn drain_burst_into(
    buf: &mut String,
    last_was_cr: &mut bool,
    first: CtEvent,
    mut drain: impl FnMut() -> Option<CtEvent>,
) -> Option<CtEvent> {
    match coalescible_char(&first) {
        Some(c) => push_burst_char(buf, last_was_cr, c),
        None => return Some(first),
    }
    while let Some(evt) = drain() {
        // Skip key release/repeat without ending the burst.
        if let CtEvent::Key(k) = &evt
            && k.kind != KeyEventKind::Press
        {
            continue;
        }
        match coalescible_char(&evt) {
            Some(c) => push_burst_char(buf, last_was_cr, c),
            None => return Some(evt),
        }
    }
    None
}

/// How long a paste-shaped burst survives a momentary quiet before it is
/// considered finished.
///
/// ConPTY hands a single terminal paste to the reader in several chunks with
/// sub-millisecond to few-millisecond gaps; 25ms absorbs those (and
/// loaded-runner jitter) while staying far below human reaction time, so a
/// deliberate keystroke after a paste does not land inside the window in
/// practice. The wait is paid only while a burst is already paste-shaped — a
/// lone keystroke, Enter included, never waits.
const PASTE_CHUNK_BRIDGE: std::time::Duration = std::time::Duration::from_millis(25);

/// Extend a just-coalesced paste across chunk gaps.
///
/// Waits up to [`PASTE_CHUNK_BRIDGE`] for more input, folding further
/// coalescible chunks into `text`, until the quiet outlasts the window or
/// deliberate input arrives (returned, to be processed after the merged
/// paste).
///
/// This is the second half of [`coalesce_key_burst`]: its drain sees only
/// *immediately available* events, so a chunk gap ends the burst — and a
/// pasted newline arriving just past the gap lands as a lone Enter, which is
/// a submit. Pasting a three-line prompt fired two half-prompts at the model
/// (#351). Inside the bridge window an Enter — Ctrl+Enter included — is
/// paste content, never a submit; a deliberate submit after a paste arrives
/// on a human timescale, not within milliseconds of the last chunk.
///
/// `last_was_cr` seeds the CRLF fold state from the burst this paste came
/// from, so a pair split exactly at the chunk gap (CR closing one chunk, LF
/// opening the next — precisely where ConPTY splits) still folds to one
/// newline.
#[must_use]
pub async fn bridge_paste_chunks<S>(
    text: &mut String,
    mut last_was_cr: bool,
    events: &mut S,
) -> Vec<Msg>
where
    S: futures::Stream<Item = std::io::Result<CtEvent>> + Unpin,
{
    use futures::{FutureExt, StreamExt};
    loop {
        let Ok(arrival) = tokio::time::timeout(PASTE_CHUNK_BRIDGE, events.next()).await else {
            // The quiet outlasted the window: the paste is over.
            return Vec::new();
        };
        let Some(Ok(evt)) = arrival else {
            // Stream end or error; the main loop observes it on its next poll.
            return Vec::new();
        };
        // Skip key release/repeat without ending the bridge — the same rule
        // the coalescer's drain applies mid-burst.
        if let CtEvent::Key(k) = &evt
            && k.kind != KeyEventKind::Press
        {
            continue;
        }
        if coalescible_char(&evt).is_none() {
            // Deliberate input: the paste is over; hand the event back.
            return event_to_msg(evt).into_iter().collect();
        }
        // A continuation chunk: fold it — and everything immediately behind
        // it — straight into the paste. Even a single-key chunk is paste
        // content here; the single-keystroke rule belongs to burst STARTS,
        // not to continuations inside the window.
        let breaker = drain_burst_into(text, &mut last_was_cr, evt, || {
            events.next().now_or_never().flatten().and_then(Result::ok)
        });
        if let Some(b) = breaker {
            // The continuation stopped on deliberate input: the paste is over.
            return event_to_msg(b).into_iter().collect();
        }
    }
}

/// What one key press contributes to a coalesced paste.
///
/// Newlines carry their source byte because ConPTY re-encodes a pasted `\r`
/// as plain Enter and a pasted `\n` as **Ctrl+Enter** — so a CRLF pair
/// arrives as two Enter presses that must fold to ONE newline, while a
/// bare-LF line ending (Ctrl+Enter with no preceding plain Enter) is a real
/// newline of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BurstChar {
    /// A printable char or a pasted tab.
    Plain(char),
    /// Enter without Control: a pasted `\r` (or the user's Enter key).
    CrNewline,
    /// Enter with Control: a pasted `\n`. Recorded distinctly so the fold
    /// can collapse the LF half of a CRLF pair.
    LfNewline,
}

/// Fold one burst character into `buf`. `last_was_cr` carries across calls
/// within a single accumulation, so `\r\n` folds to one newline while bare
/// `\n`s and blank lines (`\r\n\r\n`) keep their count.
fn push_burst_char(buf: &mut String, last_was_cr: &mut bool, ch: BurstChar) {
    match ch {
        BurstChar::Plain(c) => {
            buf.push(c);
            *last_was_cr = false;
        },
        BurstChar::CrNewline => {
            buf.push('\n');
            *last_was_cr = true;
        },
        BurstChar::LfNewline => {
            if !*last_was_cr {
                buf.push('\n');
            }
            *last_was_cr = false;
        },
    }
}

/// The contribution a key press makes to a coalesced paste, or `None` when
/// the event isn't part of a paste burst.
///
/// Unmodified `Char`, `Enter`, and `Tab` presses qualify — and so does
/// **Enter with Control**, because that is how ConPTY delivers a pasted LF
/// byte (issue #351: treating it as deliberate input broke the burst, and
/// the reducer's Enter arm then submitted half the paste). A LONE
/// Ctrl+Enter still submits: the caller's single-keystroke rule returns it
/// as a normal key when no burst formed around it. Every other modifier
/// combination stays non-coalescible, so Ctrl+C still interrupts and
/// chords keep their meaning mid-paste.
fn coalescible_char(event: &CtEvent) -> Option<BurstChar> {
    let CtEvent::Key(key) = event else {
        return None;
    };
    if key.kind != KeyEventKind::Press {
        return None;
    }
    if key.modifiers.intersects(CtMods::ALT) {
        return None;
    }
    if key.modifiers.intersects(CtMods::CONTROL) {
        return (key.code == CtKeyCode::Enter).then_some(BurstChar::LfNewline);
    }
    match key.code {
        CtKeyCode::Char(c) => Some(BurstChar::Plain(c)),
        CtKeyCode::Enter => Some(BurstChar::CrNewline),
        // Pasted tabs arrive as Tab key events on the Windows console; fold
        // them into the burst so indented code survives a paste. A lone Tab
        // (no burst) still falls through to the normal key path below.
        CtKeyCode::Tab => Some(BurstChar::Plain('\t')),
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

#[cfg(test)]
mod tests {
    use super::*;
    use mermaid_domain::SlashCmd;

    #[test]
    fn parses_theme_and_editor_commands() {
        assert_eq!(
            mermaid_domain::parse_slash_command("theme"),
            SlashCmd::Theme(None)
        );
        assert_eq!(
            mermaid_domain::parse_slash_command("theme light"),
            SlashCmd::Theme(Some("light".to_string()))
        );
        assert_eq!(
            mermaid_domain::parse_slash_command("editor"),
            SlashCmd::Editor
        );
    }

    #[test]
    fn parses_agents_command_with_kill_tail() {
        assert_eq!(
            mermaid_domain::parse_slash_command("agents"),
            SlashCmd::Agents(None)
        );
        assert_eq!(
            mermaid_domain::parse_slash_command("agents kill a1"),
            SlashCmd::Agents(Some("kill a1".to_string()))
        );
        assert_eq!(
            mermaid_domain::parse_slash_command("agents kill all"),
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

    /// ConPTY delivers a pasted LF byte as Enter+CONTROL. Inside a burst it
    /// is a newline, not deliberate input — treating it as deliberate broke
    /// the burst and the reducer's Enter arm submitted half the paste (#351).
    #[test]
    fn ctrl_enter_in_a_burst_folds_as_a_newline() {
        let mut rest = vec![
            key_with(CtKeyCode::Enter, CtMods::CONTROL, KeyEventKind::Press),
            key(CtKeyCode::Char('b')),
        ]
        .into_iter();
        let (primary, trailing) = coalesce_key_burst(key(CtKeyCode::Char('a')), || rest.next());
        assert!(
            matches!(primary, Some(Msg::Paste(Paste::Text(ref s))) if s == "a\nb"),
            "got {primary:?}"
        );
        assert!(trailing.is_empty());
    }

    /// A CRLF pair arrives as Enter (the CR) then Ctrl+Enter (the LF) —
    /// one line ending, one newline.
    #[test]
    fn a_crlf_pair_in_a_burst_folds_to_one_newline() {
        let mut rest = vec![
            key(CtKeyCode::Enter),
            key_with(CtKeyCode::Enter, CtMods::CONTROL, KeyEventKind::Press),
            key(CtKeyCode::Char('b')),
        ]
        .into_iter();
        let (primary, _) = coalesce_key_burst(key(CtKeyCode::Char('a')), || rest.next());
        assert!(
            matches!(primary, Some(Msg::Paste(Paste::Text(ref s))) if s == "a\nb"),
            "CRLF must fold to one newline, got {primary:?}"
        );
    }

    /// Two CRLF pairs are a blank line; two bare LFs are one too. The dedup
    /// must collapse pairs, never runs.
    #[test]
    fn blank_lines_survive_the_crlf_fold() {
        // a\r\n\r\nb — a blank line between a and b.
        let mut rest = vec![
            key(CtKeyCode::Enter),
            key_with(CtKeyCode::Enter, CtMods::CONTROL, KeyEventKind::Press),
            key(CtKeyCode::Enter),
            key_with(CtKeyCode::Enter, CtMods::CONTROL, KeyEventKind::Press),
            key(CtKeyCode::Char('b')),
        ]
        .into_iter();
        let (primary, _) = coalesce_key_burst(key(CtKeyCode::Char('a')), || rest.next());
        assert!(
            matches!(primary, Some(Msg::Paste(Paste::Text(ref s))) if s == "a\n\nb"),
            "got {primary:?}"
        );

        // a\n\nb with bare-LF endings: two Ctrl+Enters, two newlines.
        let mut rest = vec![
            key_with(CtKeyCode::Enter, CtMods::CONTROL, KeyEventKind::Press),
            key_with(CtKeyCode::Enter, CtMods::CONTROL, KeyEventKind::Press),
            key(CtKeyCode::Char('b')),
        ]
        .into_iter();
        let (primary, _) = coalesce_key_burst(key(CtKeyCode::Char('a')), || rest.next());
        assert!(
            matches!(primary, Some(Msg::Paste(Paste::Text(ref s))) if s == "a\n\nb"),
            "bare LFs each count, got {primary:?}"
        );
    }

    /// A deliberate Ctrl+Enter (no burst around it) keeps its key meaning —
    /// the single-keystroke rule, not the fold, decides it.
    #[test]
    fn coalesce_lone_ctrl_enter_stays_a_key() {
        let (primary, _) = coalesce_key_burst(
            key_with(CtKeyCode::Enter, CtMods::CONTROL, KeyEventKind::Press),
            || None,
        );
        assert!(
            matches!(primary, Some(Msg::Key(k)) if k.code == KeyCode::Enter && k.modifiers.ctrl),
            "got {primary:?}"
        );
    }

    /// Scripted event stream for bridge tests: each entry arrives after its
    /// virtual delay. Under a paused clock the delays are exact. Fused,
    /// because the bridge keeps polling until the window closes — the real
    /// `EventStream` is channel-backed and re-poll-safe the same way.
    fn scripted(
        events: Vec<(u64, CtEvent)>,
    ) -> std::pin::Pin<Box<dyn futures::Stream<Item = std::io::Result<CtEvent>> + Send>> {
        use futures::StreamExt;
        Box::pin(
            futures::stream::unfold(events.into_iter(), |mut it| async move {
                let (delay_ms, evt) = it.next()?;
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                Some((Ok(evt), it))
            })
            .fuse(),
        )
    }

    /// The #351 shape: a chunk gap, then the rest of the paste. The newline
    /// and following text must fold into the paste, not submit it.
    #[tokio::test(start_paused = true)]
    async fn bridge_folds_a_chunk_gap_newline_into_the_paste() {
        let mut text = String::from("line one");
        let mut stream = scripted(vec![
            (
                2,
                key_with(CtKeyCode::Enter, CtMods::CONTROL, KeyEventKind::Press),
            ),
            (1, key(CtKeyCode::Char('x'))),
        ]);
        let trailing = bridge_paste_chunks(&mut text, false, &mut stream).await;
        assert_eq!(text, "line one\nx");
        assert!(trailing.is_empty());
    }

    /// The seam ConPTY actually splits at: the CR closed the coalesced burst
    /// (fold state seeded true), its LF opens the bridge. One newline, not
    /// two.
    #[tokio::test(start_paused = true)]
    async fn bridge_dedups_a_crlf_pair_split_at_the_seam() {
        let mut text = String::from("line one\n");
        let mut stream = scripted(vec![
            (
                2,
                key_with(CtKeyCode::Enter, CtMods::CONTROL, KeyEventKind::Press),
            ),
            (1, key(CtKeyCode::Char('x'))),
        ]);
        let trailing = bridge_paste_chunks(&mut text, true, &mut stream).await;
        assert_eq!(text, "line one\nx", "the split CRLF must stay one newline");
        assert!(trailing.is_empty());
    }

    /// Quiet longer than the window ends the paste; the late keystroke stays
    /// in the stream for the main loop and is NOT folded into the paste.
    #[tokio::test(start_paused = true)]
    async fn bridge_gives_up_when_the_quiet_outlasts_the_window() {
        use futures::StreamExt;
        let mut text = String::from("chunk");
        let mut stream = scripted(vec![(50, key(CtKeyCode::Char('x')))]);
        let trailing = bridge_paste_chunks(&mut text, false, &mut stream).await;
        assert_eq!(text, "chunk", "a 50ms-late keystroke is typing, not paste");
        assert!(trailing.is_empty());
        let late = stream.next().await;
        assert!(
            matches!(late, Some(Ok(CtEvent::Key(k))) if k.code == CtKeyCode::Char('x')),
            "the late keystroke must still be deliverable, got {late:?}"
        );
    }

    /// Deliberate input inside the window ends the paste and is handed back
    /// to be processed after it.
    #[tokio::test(start_paused = true)]
    async fn bridge_hands_back_deliberate_input_inside_the_window() {
        let mut text = String::from("pasted");
        let mut stream = scripted(vec![(2, key(CtKeyCode::Esc))]);
        let trailing = bridge_paste_chunks(&mut text, false, &mut stream).await;
        assert_eq!(text, "pasted");
        assert_eq!(trailing.len(), 1);
        assert!(matches!(trailing[0], Msg::Key(k) if k.code == KeyCode::Escape));
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
        assert_eq!(
            mermaid_domain::parse_slash_command("model"),
            SlashCmd::Model(None)
        );
    }

    #[test]
    fn parse_slash_model_with_arg() {
        assert_eq!(
            mermaid_domain::parse_slash_command("model anthropic/opus"),
            SlashCmd::Model(Some("anthropic/opus".to_string())),
        );
    }

    #[test]
    fn parse_slash_quit_alias_q() {
        assert_eq!(mermaid_domain::parse_slash_command("q"), SlashCmd::Quit);
    }

    #[test]
    fn parse_slash_usage_and_context() {
        use mermaid_domain::ContextCmd;
        assert_eq!(
            mermaid_domain::parse_slash_command("usage"),
            SlashCmd::Usage
        );
        assert_eq!(
            mermaid_domain::parse_slash_command("context"),
            SlashCmd::Context(ContextCmd::Show)
        );
        assert_eq!(
            mermaid_domain::parse_slash_command("context 65536"),
            SlashCmd::Context(ContextCmd::Set(65536))
        );
        assert_eq!(
            mermaid_domain::parse_slash_command("context auto"),
            SlashCmd::Context(ContextCmd::Auto)
        );
        assert_eq!(
            mermaid_domain::parse_slash_command("context max"),
            SlashCmd::Context(ContextCmd::Max)
        );
        assert_eq!(
            mermaid_domain::parse_slash_command("context offload on"),
            SlashCmd::Context(ContextCmd::Offload(true))
        );
        assert_eq!(
            mermaid_domain::parse_slash_command("context offload off"),
            SlashCmd::Context(ContextCmd::Offload(false))
        );
        // Unrecognized arg falls back to the (self-documenting) report.
        assert_eq!(
            mermaid_domain::parse_slash_command("context wat"),
            SlashCmd::Context(ContextCmd::Show)
        );
        assert_eq!(
            mermaid_domain::parse_slash_command("doctor"),
            SlashCmd::Doctor
        );
    }

    #[test]
    fn parse_slash_compact_and_aliases() {
        assert_eq!(
            mermaid_domain::parse_slash_command("compact"),
            SlashCmd::Compact(None)
        );
        assert_eq!(
            mermaid_domain::parse_slash_command("compact focus on tests"),
            SlashCmd::Compact(Some("focus on tests".to_string()))
        );
        assert_eq!(
            mermaid_domain::parse_slash_command("compress"),
            SlashCmd::Compact(None)
        );
        assert_eq!(
            mermaid_domain::parse_slash_command("summarize"),
            SlashCmd::Compact(None)
        );
    }

    #[test]
    fn parse_memory_commands() {
        assert_eq!(
            mermaid_domain::parse_slash_command("memory"),
            SlashCmd::Memory
        );
        assert_eq!(
            mermaid_domain::parse_slash_command("memories"),
            SlashCmd::Memory
        ); // alias
        assert_eq!(
            mermaid_domain::parse_slash_command("remember prefer ripgrep"),
            SlashCmd::Remember("prefer ripgrep".to_string())
        );
        assert_eq!(
            mermaid_domain::parse_slash_command("remember"),
            SlashCmd::MissingArg("Usage: /remember <fact>".to_string()),
            "a bare required-arg command answers with the registry usage line"
        );
        assert_eq!(
            mermaid_domain::parse_slash_command("forget prefer-ripgrep"),
            SlashCmd::Forget("prefer-ripgrep".to_string())
        );
        assert_eq!(
            mermaid_domain::parse_slash_command("forget"),
            SlashCmd::MissingArg("Usage: /forget <name> (see /memory for names)".to_string()),
            "the usage note rides along from the registry"
        );
        assert_eq!(
            mermaid_domain::parse_slash_command("consolidate-memory"),
            SlashCmd::ConsolidateMemory
        );
        assert_eq!(
            mermaid_domain::parse_slash_command("prune-memory"),
            SlashCmd::ConsolidateMemory
        ); // alias
    }

    #[test]
    fn parse_runtime_task_commands() {
        assert_eq!(
            mermaid_domain::parse_slash_command("tasks"),
            SlashCmd::Tasks
        );
        assert_eq!(
            mermaid_domain::parse_slash_command("task task-123"),
            SlashCmd::Task("task-123".to_string())
        );
        assert_eq!(
            mermaid_domain::parse_slash_command("pause task-123"),
            SlashCmd::Pause("task-123".to_string())
        );
        assert_eq!(
            mermaid_domain::parse_slash_command("resume task-123"),
            SlashCmd::Resume("task-123".to_string())
        );
        assert_eq!(
            mermaid_domain::parse_slash_command("cancel"),
            SlashCmd::Cancel(None)
        );
        assert_eq!(
            mermaid_domain::parse_slash_command("handoff task-123"),
            SlashCmd::Handoff(Some("task-123".to_string()))
        );
        assert_eq!(
            mermaid_domain::parse_slash_command("report"),
            SlashCmd::Report(None)
        );
        assert_eq!(
            mermaid_domain::parse_slash_command("procs"),
            SlashCmd::Processes
        );
        assert_eq!(
            mermaid_domain::parse_slash_command("approvals"),
            SlashCmd::Approvals
        );
        assert_eq!(
            mermaid_domain::parse_slash_command("approve approval-1"),
            SlashCmd::Approve("approval-1".to_string())
        );
        assert_eq!(
            mermaid_domain::parse_slash_command("deny approval-1"),
            SlashCmd::Deny("approval-1".to_string())
        );
        assert_eq!(
            mermaid_domain::parse_slash_command("checkpoint src/lib.rs"),
            SlashCmd::Checkpoint("src/lib.rs".to_string())
        );
        assert_eq!(
            mermaid_domain::parse_slash_command("checkpoints"),
            SlashCmd::Checkpoints
        );
        assert_eq!(
            mermaid_domain::parse_slash_command("restore checkpoint-1"),
            SlashCmd::Restore("checkpoint-1".to_string())
        );
        assert_eq!(
            mermaid_domain::parse_slash_command("plugins"),
            SlashCmd::Plugins
        );
    }

    #[test]
    fn parse_slash_reasoning_valid_level() {
        assert_eq!(
            mermaid_domain::parse_slash_command("reasoning high"),
            SlashCmd::Reasoning(Some(mermaid_model::models::ReasoningLevel::High)),
        );
    }

    #[test]
    fn parse_slash_visible_reasoning_and_alias() {
        assert_eq!(
            mermaid_domain::parse_slash_command("visible-reasoning on"),
            SlashCmd::VisibleReasoning(Some("on".to_string())),
        );
        assert_eq!(
            mermaid_domain::parse_slash_command("visiblereasoning"),
            SlashCmd::VisibleReasoning(None),
        );
    }

    #[test]
    fn parse_slash_reasoning_invalid_level_is_none_arg() {
        // Argument exists but can't be parsed to a level — degrades
        // to showing current (None arg) rather than erroring.
        assert_eq!(
            mermaid_domain::parse_slash_command("reasoning bogus"),
            SlashCmd::Reasoning(None),
        );
    }

    #[test]
    fn parse_safety_command() {
        assert_eq!(
            mermaid_domain::parse_slash_command("safety auto"),
            SlashCmd::Safety(Some(mermaid_runtime::SafetyMode::Auto)),
        );
        // `/permission` is an alias that routes to the same command.
        assert_eq!(
            mermaid_domain::parse_slash_command("permission read_only"),
            SlashCmd::Safety(Some(mermaid_runtime::SafetyMode::ReadOnly)),
        );
        // No arg → show current; bogus value → None (show current + options).
        assert_eq!(
            mermaid_domain::parse_slash_command("safety"),
            SlashCmd::Safety(None)
        );
        assert_eq!(
            mermaid_domain::parse_slash_command("safety bogus"),
            SlashCmd::Safety(None)
        );
    }

    #[test]
    fn parse_slash_unknown_command() {
        match mermaid_domain::parse_slash_command("nope") {
            SlashCmd::Unknown(name) => assert_eq!(name, "nope"),
            other => panic!("expected Unknown, got {other:?}"),
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
