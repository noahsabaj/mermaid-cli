//! Pure view: `fn render(&State, &mut Frame)`.
//!
//! Three contracts:
//!   1. Never mutates `State`. The view is fully derived.
//!   2. Never performs I/O. All state — model lists, MCP status,
//!      file contents — is whatever the reducer put in `State`.
//!   3. Never holds a `&mut App` / `&mut anything` other than the
//!      `Frame` ratatui owns. No state mutation through back channels
//!      like the old `tui::render` did.
//!
//! Those three rules make the renderer testable without a runtime,
//! without a terminal, and without even real input — feed a mock
//! `State`, paint into ratatui's `TestBackend`, assert on cell
//! contents.

pub mod attachments;
pub mod chat;
pub mod input;
pub mod layout;
pub mod palette;
pub mod status;

use ratatui::Frame;

use crate::domain::State;

/// The entrypoint. Call once per render pass from the main loop.
pub fn render(state: &State, frame: &mut Frame) {
    let area = frame.area();
    let zones = layout::Zones::for_state(area, state);

    chat::render_chat(state, frame, zones.chat);
    attachments::render_attachments(state, frame, zones.attachments);
    input::render_input(state, frame, zones.input);
    status::render_status(state, frame, zones.status);
    palette::maybe_render_palette(state, frame, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Config;
    use crate::domain::{State, StatusKind, StatusLine, TurnState};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;

    fn mock_state() -> State {
        State::new(
            Config::default(),
            PathBuf::from("/tmp/p"),
            "ollama/test".to_string(),
        )
    }

    fn render_to_string(state: &State) -> String {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|f| render(state, f)).expect("draw");
        let buf = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn idle_state_renders_hint() {
        let s = mock_state();
        let frame = render_to_string(&s);
        assert!(frame.contains("Ask me anything"));
        assert!(frame.contains("ollama/test"));
    }

    #[test]
    fn generating_state_shows_sending_hint() {
        let mut s = mock_state();
        s.turn = crate::domain::transition::start_generating(crate::domain::TurnId(1));
        let frame = render_to_string(&s);
        assert!(frame.contains("sending") || frame.contains("working"));
    }

    #[test]
    fn status_line_rendered_when_present() {
        let mut s = mock_state();
        s.status = Some(StatusLine {
            text: "unique-status-test-token".to_string(),
            kind: StatusKind::Warn,
            shown_at: std::time::SystemTime::now(),
        });
        let frame = render_to_string(&s);
        assert!(frame.contains("unique-status-test-token"));
    }

    #[test]
    fn committed_message_appears_in_chat_pane() {
        let mut s = mock_state();
        s.session
            .append(crate::models::ChatMessage::user("unique-user-token-xyz"));
        let frame = render_to_string(&s);
        assert!(frame.contains("unique-user-token-xyz"));
    }

    #[test]
    fn cancelling_state_shows_cancelling_title() {
        let mut s = mock_state();
        s.turn = TurnState::Cancelling {
            id: crate::domain::TurnId(1),
            since: std::time::SystemTime::now(),
        };
        let frame = render_to_string(&s);
        assert!(frame.contains("cancelling"));
    }
}
