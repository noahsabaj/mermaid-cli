//! The two dim lines an empty transcript opens with: what this session is
//! (version, model, directory) and how to drive it. They disappear with the
//! first message, so a running conversation never carries them, and they come
//! back on `/clear`.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use std::path::{Path, PathBuf};
use unicode_width::UnicodeWidthStr;

use super::truncate_to_cells;
use crate::render::theme::Theme;
use mermaid_domain::{State, TurnState};
use mermaid_model::models::MessageRole;

/// Rows the header takes when visible.
pub const SESSION_HEADER_HEIGHT: u16 = 2;

/// Visible while every committed message is a system notice and no turn is
/// running. Not `messages().is_empty()`: the startup web-capability notice is
/// a system message that only appears on degraded machines, and keying on it
/// would make the header (and the pty frames CI compares) environment-
/// dependent. `/clear` brings the header back; `--resume` with content hides it.
#[must_use]
pub fn session_header_visible(state: &State) -> bool {
    matches!(state.turn, TurnState::Idle)
        && state
            .session
            .messages()
            .iter()
            .all(|message| message.role == MessageRole::System)
}

/// The two header lines, both in the meta colour, each fitted to `width`.
#[must_use]
pub fn build_session_header(
    version: &str,
    model_id: &str,
    cwd: &Path,
    home: Option<&Path>,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let meta = Style::new().fg(theme.colors.text_meta.to_color());
    let identity = format!(
        "mermaid v{version} · {model_id} · {}",
        abbreviate_home(cwd, home).display()
    );
    let identity = fit_left(&identity, width);
    let hints = truncate_to_cells(
        "/help for commands · shift+tab cycles safety · esc interrupts",
        width,
    );
    vec![
        Line::from(Span::styled(identity, meta)),
        Line::from(Span::styled(hints, meta)),
    ]
}

/// `~` for the home directory itself, `~/rest` beneath it (joined, so Windows
/// keeps its separator), the path unchanged otherwise.
#[must_use]
pub fn abbreviate_home(cwd: &Path, home: Option<&Path>) -> PathBuf {
    match home {
        Some(home) if cwd == home => PathBuf::from("~"),
        Some(home) => match cwd.strip_prefix(home) {
            Ok(rest) => PathBuf::from("~").join(rest),
            Err(_) => cwd.to_path_buf(),
        },
        None => cwd.to_path_buf(),
    }
}

/// Truncate from the LEFT so a long path keeps its tail, which is the part
/// that tells one checkout from another.
fn fit_left(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    let keep = width.saturating_sub(1);
    let mut tail = String::new();
    let mut used = 0;
    for ch in text.chars().rev() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > keep {
            break;
        }
        used += w;
        tail.insert(0, ch);
    }
    format!("…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_header_abbreviates_home() {
        let home = Path::new("/home/u");
        assert_eq!(
            abbreviate_home(Path::new("/home/u/x"), Some(home)),
            PathBuf::from("~").join("x")
        );
        assert_eq!(abbreviate_home(home, Some(home)), PathBuf::from("~"));
        assert_eq!(
            abbreviate_home(Path::new("/srv/x"), Some(home)),
            PathBuf::from("/srv/x")
        );
        assert_eq!(
            abbreviate_home(Path::new("/srv/x"), None),
            PathBuf::from("/srv/x")
        );
    }

    #[test]
    fn a_long_path_keeps_its_tail() {
        let theme = Theme::dark();
        let deep = PathBuf::from("/".to_string() + &"segment/".repeat(40) + "mermaid-cli");
        let lines = build_session_header("0.0.0", "ollama/test", &deep, None, 60, &theme);
        let first: String = lines[0]
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(first.width() <= 60, "{first}");
        assert!(first.ends_with("mermaid-cli"), "{first}");
        assert!(first.starts_with('…'), "{first}");
    }
}
