//! Which surface owns the composer buffer: the slash-command palette, or the
//! model.
//!
//! One decision, made once. Six call sites used to answer it independently
//! with a bare `input_buffer.starts_with('/')` — the reducer's palette key
//! interception, three `palette_cursor` resets, the paste path, the render
//! layer's border cue and bottom pane, and the @-mention suppressor — and two
//! of them stripped a DIFFERENT number of slashes than the submit path did
//! (`trim_start_matches('/')` against `strip_prefix('/')`).
//!
//! The visible cost: typing `/home/you/pkg.deb can you make this run on
//! fedora` turned the composer yellow and retitled it " Enter Command ",
//! replaced the status band with an empty palette, hijacked Up/Down/Tab/Esc
//! (Esc would have wiped the line), hid the @-picker, and on Enter discarded
//! the whole message for a transcript row reading
//! `Unknown command: /home/you/pkg.deb`.
//!
//! The rule is **registry membership**, not a path heuristic: a `/`-prefixed
//! buffer is a command exactly when its first word names a real command. That
//! subsumes paths without having to recognize one — `home/you/pkg.deb` names
//! no command, so it is prose. It also removes "unknown command" as a
//! reachable state: anything the registry does not know is a message, so
//! nothing the user typed is ever discarded.

use crate::slash_commands::{COMMAND_REGISTRY, PaletteEntry, SlashCommand, filter_entries};

/// A `/`-prefixed buffer, split at the command word exactly once.
///
/// The ONE prefix-strip in the codebase. Every consumer takes its slice from
/// here rather than re-splitting, which is what stops the palette and the
/// dispatcher from disagreeing about how many slashes a line has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandLine<'a> {
    /// Everything after the single leading `/` — exactly what
    /// [`crate::parse_slash_command`] takes.
    pub rest: &'a str,
    /// The command word: `rest` up to the first whitespace. Empty for a bare
    /// `/`, which is how `/` alone still opens the whole palette.
    pub token: &'a str,
    /// Everything after the command word, its leading whitespace kept, so
    /// `format!("/{token}{args}")` round-trips the line.
    pub args: &'a str,
}

/// Split a composer buffer at its command word, or `None` when the buffer
/// does not open with `/`.
///
/// The `/` must be at byte 0. A leading space makes the line prose — the
/// border, the palette and Enter then agree, where before a buffer of
/// `"  /help"` drew no palette but still ran the command on submit.
#[must_use]
pub fn command_line(buf: &str) -> Option<CommandLine<'_>> {
    let rest = buf.strip_prefix('/')?;
    let (token, args) = match rest.find(char::is_whitespace) {
        Some(idx) => rest.split_at(idx),
        None => (rest, ""),
    };
    Some(CommandLine { rest, token, args })
}

/// The registry entry a command word names, if any.
///
/// Case-insensitive and EXACT: a prefix belongs to the palette, not to
/// dispatch. `/mo` is prose until the palette completes it to `/model`.
#[must_use]
pub fn builtin_named(token: &str) -> Option<&'static SlashCommand> {
    let name = token.to_lowercase();
    COMMAND_REGISTRY
        .iter()
        .find(|c| c.name == name || c.aliases.contains(&name.as_str()))
}

/// What the composer buffer is, and everything the caller needs to act on it.
///
/// Borrowed, never owned: the render layer classifies on every frame and the
/// reducer on every keystroke, and nothing stores the answer.
#[derive(Debug)]
pub enum InputKind<'a, 'p> {
    /// A built-in command. `rest` is everything after the leading `/`, ready
    /// for [`crate::parse_slash_command`].
    Builtin { rest: &'a str },
    /// An enabled plugin's prompt command, with the args to expand it with.
    Plugin {
        cmd: &'p crate::PluginCommand,
        args: &'a str,
    },
    /// Prose for the model. Carries no payload because it always means the
    /// WHOLE buffer, verbatim, leading `/` included — callers take the buffer
    /// rather than copying a slice out of it.
    Text,
}

/// Classify the composer buffer. The ONLY way to ask "is this a command?".
///
/// Built-ins win over plugins, matching the loader, which already refuses a
/// plugin whose name shadows a built-in; keeping the order here too makes it
/// structural rather than a coincidence of two guards agreeing.
#[must_use]
pub fn classify_input<'a, 'p>(
    buf: &'a str,
    plugins: &'p [crate::PluginCommand],
) -> InputKind<'a, 'p> {
    let Some(line) = command_line(buf) else {
        return InputKind::Text;
    };
    if builtin_named(line.token).is_some() {
        return InputKind::Builtin { rest: line.rest };
    }
    let name = line.token.to_lowercase();
    if let Some(cmd) = plugins.iter().find(|p| p.name == name) {
        return InputKind::Plugin {
            cmd,
            args: line.args,
        };
    }
    InputKind::Text
}

/// The rows the palette should show, or `None` when it must stay closed.
///
/// Open-but-empty is not representable, and that is the point: the palette
/// rendering "No matching commands" over the status band while it stole the
/// keyboard IS the hijack this module exists to delete. A caller either has
/// rows to draw or has no palette.
#[must_use]
pub fn palette_rows<'p>(
    buf: &str,
    plugins: &'p [crate::PluginCommand],
) -> Option<Vec<PaletteEntry<'p>>> {
    let line = command_line(buf)?;
    let rows = filter_entries(line.token, plugins);
    (!rows.is_empty()).then_some(rows)
}

/// Whether the palette owns the composer: it has at least one row to offer.
///
/// Drives the key interception, the `palette_cursor` resets, the border cue
/// and the @-mention suppressor. Deliberately routed through
/// [`palette_rows`] rather than reimplementing the prefix test — a second
/// copy of that predicate is exactly the defect this module removes.
#[must_use]
pub fn palette_is_open(buf: &str, plugins: &[crate::PluginCommand]) -> bool {
    palette_rows(buf, plugins).is_some()
}

/// The `/word` to name in the "no commands match" hint, or `None` when the
/// buffer is not slash-prefixed at all. Only meaningful while
/// [`palette_is_open`] is false — that pairing is what the hint reports.
#[must_use]
pub fn unmatched_command_word(buf: &str) -> Option<String> {
    let line = command_line(buf)?;
    Some(format!("/{}", line.token))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact buffer from the bug report.
    const DEB: &str =
        "/home/nsabaj/Downloads/grok-bot_0.44.0_amd64.deb can you make this run on fedora";

    fn plugin(name: &str) -> crate::PluginCommand {
        crate::PluginCommand {
            name: name.to_string(),
            description: "does things".to_string(),
            body: "body".to_string(),
            plugin: "demo".to_string(),
        }
    }

    fn is_text(buf: &str) -> bool {
        matches!(classify_input(buf, &[]), InputKind::Text)
    }

    #[test]
    fn absolute_path_is_text_not_a_command() {
        assert!(is_text(DEB), "a path names no command, so it is a message");
        assert!(
            !palette_is_open(DEB, &[]),
            "and the palette must not open over it"
        );
    }

    #[test]
    fn dotted_and_nested_first_tokens_are_text() {
        for buf in ["/notes.md", "/etc/hosts", "/usr/local/bin/thing", "/x.deb"] {
            assert!(is_text(buf), "{buf} names no command");
        }
    }

    #[test]
    fn bare_unknown_word_is_text() {
        // The remainder the old path-shaped rule could not reach: a
        // single-segment absolute path is word-like, so only registry
        // membership can tell it from a command.
        assert!(is_text("/tmp is full of junk"));
        assert!(is_text("/opt"));
    }

    #[test]
    fn a_typo_is_text_and_never_dispatches() {
        assert!(is_text("/compct now"), "a typo must not run /compact");
        assert!(
            !palette_is_open("/compct now", &[]),
            "and the hint, not the palette, is what tells the user"
        );
    }

    #[test]
    fn every_registry_name_and_alias_is_a_command() {
        // The invariant that makes registry membership a safe test: no
        // reachable command can classify as prose. Registry-driven, so a
        // command added tomorrow is covered the day it lands.
        for cmd in COMMAND_REGISTRY {
            for name in std::iter::once(&cmd.name).chain(cmd.aliases.iter()) {
                let buf = format!("/{name}");
                assert!(
                    matches!(classify_input(&buf, &[]), InputKind::Builtin { .. }),
                    "/{name} must dispatch"
                );
                assert!(palette_is_open(&buf, &[]), "/{name} must offer a row");
            }
        }
    }

    #[test]
    fn a_name_is_matched_case_insensitively() {
        assert!(matches!(
            classify_input("/HELP", &[]),
            InputKind::Builtin { .. }
        ));
    }

    #[test]
    fn slashes_in_an_argument_do_not_disqualify_the_command() {
        // Only the FIRST token decides; a model id is full of slashes.
        assert!(matches!(
            classify_input("/model anthropic/claude-opus-4-5", &[]),
            InputKind::Builtin { rest } if rest == "model anthropic/claude-opus-4-5"
        ));
    }

    #[test]
    fn bare_slash_opens_the_whole_palette() {
        let line = command_line("/").expect("slash-prefixed");
        assert_eq!(line.token, "", "a bare slash has an empty command word");
        assert_eq!(
            palette_rows("/", &[]).expect("rows").len(),
            COMMAND_REGISTRY.len(),
            "and offers every command"
        );
    }

    #[test]
    fn palette_rows_is_none_rather_than_empty() {
        // Open-but-empty is the hijack; it must not be representable.
        assert!(palette_rows(DEB, &[]).is_none());
        assert!(palette_rows("no slash here", &[]).is_none());
        for rows in [palette_rows("/mo", &[]), palette_rows("/", &[])] {
            assert!(!rows.expect("rows").is_empty());
        }
    }

    #[test]
    fn a_prefix_offers_rows_but_does_not_dispatch() {
        assert!(palette_is_open("/mo", &[]), "/mo must still filter");
        assert!(is_text("/mo"), "but only the palette may complete it");
    }

    #[test]
    fn double_slash_is_prose_and_strips_nothing() {
        // The old code disagreed with itself here: submit read `//foo` as the
        // command `/foo` while the palette filtered on `foo` and would
        // Tab-complete it to `/forget`.
        assert_eq!(command_line("//foo").expect("prefixed").token, "/foo");
        assert!(is_text("//foo"));
        assert!(!palette_is_open("//foo", &[]));
    }

    #[test]
    fn leading_whitespace_is_never_a_command() {
        // Before, these drew no palette and no border cue yet still ran the
        // command on Enter, because submit trimmed and nothing else did.
        for buf in ["  /help", "\n/help"] {
            assert!(is_text(buf));
            assert!(!palette_is_open(buf, &[]));
        }
    }

    #[test]
    fn a_plugin_command_classifies_as_a_plugin() {
        let plugins = vec![plugin("deploy")];
        assert!(matches!(
            classify_input("/deploy prod", &plugins),
            InputKind::Plugin { cmd, args } if cmd.name == "deploy" && args == " prod"
        ));
        assert!(palette_is_open("/dep", &plugins));
    }

    #[test]
    fn builtin_wins_over_a_same_named_plugin() {
        let plugins = vec![plugin("help")];
        assert!(matches!(
            classify_input("/help", &plugins),
            InputKind::Builtin { .. }
        ));
    }

    #[test]
    fn token_and_args_reassemble_the_line() {
        let line = command_line("/model  anthropic/opus").expect("prefixed");
        assert_eq!(
            format!("/{}{}", line.token, line.args),
            "/model  anthropic/opus"
        );
    }

    #[test]
    fn the_hint_names_the_word_with_its_slash() {
        assert_eq!(
            unmatched_command_word("/tmp is full").as_deref(),
            Some("/tmp")
        );
        assert_eq!(unmatched_command_word("plain text"), None);
    }

    #[test]
    fn multibyte_and_empty_buffers_do_not_panic() {
        for buf in [
            "",
            "/",
            "//",
            "/\u{65e5}\u{672c}\u{8a9e}/x",
            "/h\u{e9}llo",
            "/\u{1f600}",
        ] {
            let _ = classify_input(buf, &[]);
            let _ = palette_is_open(buf, &[]);
            let _ = unmatched_command_word(buf);
        }
    }
}
