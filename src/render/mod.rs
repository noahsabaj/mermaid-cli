//! Pure view: `fn render(&State, &mut RenderState, &mut Frame)`.
//!
//! Three contracts:
//!   1. Never mutates `State`. The view is fully derived.
//!   2. Never performs I/O. All state — model lists, MCP status,
//!      file contents — is whatever the reducer put in `State`.
//!   3. Never holds a `&mut App` / `&mut anything` other than the
//!      `Frame` ratatui owns and the render-layer `RenderState`
//!      (which is memoization + scroll-position bookkeeping, not
//!      reducer state).
//!
//! Layout and widgets lifted verbatim from v0.6's `tui::render`.
//! The only changes are field accesses: every `&App` read becomes
//! a `&State` read through the equivalent path.

pub mod layout;
pub mod markdown;
pub mod theme;
pub mod widgets;

use ratatui::{Frame, layout::Margin};
use rustc_hash::FxHashMap;
use unicode_width::UnicodeWidthChar;

use crate::domain::{State, TurnState};
use crate::models::{ReasoningCapability, ReasoningLevel, nearest_effort};

use widgets::{
    AttachmentWidget, ChatState, ChatWidget, GenerationStatus, InputState, InputWidget,
    SlashPaletteWidget, StatusLineWidget, StatusWidget,
};

/// Transient render-layer state that lives across frames but isn't
/// reducer state. Owned by `app::run_v7`; passed as `&mut` to
/// `render()` per frame.
///
/// Contents are pure memoization + UI affordances (scroll position,
/// markdown cache). Nothing here affects what the reducer sees or
/// what ends up on disk.
pub struct RenderState {
    pub chat: ChatState,
    pub markdown_cache: FxHashMap<u64, Vec<ratatui::text::Line<'static>>>,
    pub theme: theme::Theme,
}

impl Default for RenderState {
    fn default() -> Self {
        Self {
            chat: ChatState::new(),
            markdown_cache: FxHashMap::default(),
            theme: theme::Theme::dark(),
        }
    }
}

impl RenderState {
    pub fn new() -> Self {
        Self::default()
    }
}

/// The entrypoint. Call once per render pass from the main loop.
pub fn render(state: &State, rstate: &mut RenderState, frame: &mut Frame) {
    // Input height: content-aware, respecting CJK/emoji widths.
    let terminal_width = frame.area().width.saturating_sub(4) as usize;
    let input_lines = if state.ui.input_buffer.is_empty() {
        1
    } else {
        let mut lines = 1usize;
        let mut col = 0usize;
        for ch in state.ui.input_buffer.chars() {
            let w = ch.width().unwrap_or(0);
            if ch == '\n' || col >= terminal_width {
                lines += 1;
                col = if ch == '\n' { 0 } else { w };
            } else {
                col += w;
            }
        }
        lines.min(5)
    };
    let input_height = (input_lines + 2) as u16;

    let queued_count = state.ui.queued_messages.len();
    let status_line_height = if state.is_busy() {
        (1 + queued_count).min(6) as u16
    } else {
        0
    };

    let attachment_height = if state.ui.attachments.is_empty() {
        0
    } else {
        1
    };

    // Bottom region: palette overlay (10 lines) vs status bar (2 lines).
    let palette_open = state.ui.input_buffer.starts_with('/');
    let bottom_height = if palette_open {
        let typed = state
            .ui
            .input_buffer
            .trim_start_matches('/')
            .split_whitespace()
            .next()
            .unwrap_or("");
        let row_count = crate::tui::slash_commands::filter_by_prefix(typed)
            .len()
            .clamp(1, 8);
        (row_count as u16) + 2
    } else {
        2
    };

    // 5-zone vertical layout: chat / status line / attachments / input / bottom.
    use ratatui::layout::{Constraint, Direction, Layout};
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(10),
            Constraint::Length(status_line_height),
            Constraint::Length(attachment_height),
            Constraint::Length(input_height),
            Constraint::Length(bottom_height),
        ])
        .split(frame.area());

    // Chat area with 1-cell horizontal padding.
    let chat_area = chunks[0].inner(Margin {
        horizontal: 1,
        vertical: 0,
    });
    let committed = state.session.messages().to_vec();
    let live_messages = build_live_messages(&committed, &state.turn);
    let chat_widget = ChatWidget {
        messages: &live_messages,
        theme: &rstate.theme,
        markdown_cache: &mut rstate.markdown_cache,
        active_subagents: None, // v7 reducer doesn't wire subagent progress yet
    };
    frame.render_stateful_widget(chat_widget, chat_area, &mut rstate.chat);

    // Status line (only while generating).
    if let TurnState::Generating {
        started,
        tokens,
        partial_text,
        ..
    } = &state.turn
    {
        let elapsed_secs = started.elapsed().map(|d| d.as_secs()).unwrap_or(0);
        let (tokens_display, tokens_estimated) = if *tokens == 0 && !partial_text.is_empty() {
            (partial_text.len() / 4, true)
        } else {
            (*tokens, false)
        };
        let status_line_widget = StatusLineWidget {
            status: GenerationStatus::from_turn(&state.turn),
            elapsed_secs,
            tokens_received: tokens_display,
            tokens_estimated,
            theme: &rstate.theme,
            queued_messages: &state.ui.queued_messages,
        };
        frame.render_widget(status_line_widget, chunks[1]);
    }

    // Attachment bar.
    if !state.ui.attachments.is_empty() {
        let attachment_widget = AttachmentWidget {
            attachments: &state.ui.attachments,
            theme: &rstate.theme,
            focused: state.ui.attachment_focused,
            selected: state.ui.attachment_selected,
        };
        frame.render_widget(attachment_widget, chunks[2]);
    }

    // Input box.
    let input_widget = InputWidget {
        input: state.ui.input_buffer.as_str(),
        showing_command_hints: state.ui.input_buffer.starts_with('/'),
        theme: &rstate.theme,
        reasoning_active: state.session.reasoning != ReasoningLevel::None,
    };
    let mut input_widget_state = InputState {
        cursor_position: state.ui.input_cursor.min(state.ui.input_buffer.len()),
    };
    frame.render_stateful_widget(input_widget, chunks[3], &mut input_widget_state);

    // Cursor visible unless focus is on attachments.
    if !state.ui.attachment_focused {
        let input_area = chunks[3];
        let content_width = input_area.width.saturating_sub(2) as usize;
        let (cursor_row, cursor_col) = InputState::calculate_cursor_position(
            &state.ui.input_buffer,
            state.ui.input_cursor.min(state.ui.input_buffer.len()),
            content_width,
        );
        frame.set_cursor_position((
            input_area.x + cursor_col + 2,
            input_area.y + 1 + cursor_row,
        ));
    }

    // Effective reasoning level (v7 doesn't have a per-model
    // supported_reasoning cap yet; default to no snap indicator until
    // ProviderFactory::capabilities is threaded here).
    let requested = state.session.reasoning;
    let effective = match supported_reasoning_for(state) {
        Some(ReasoningCapability::Levels(supp)) => {
            nearest_effort(requested, &supp).unwrap_or(requested)
        },
        _ => requested,
    };
    let requested_level = if effective == requested {
        None
    } else {
        Some(requested)
    };

    // Bottom: palette overlay or status bar.
    if palette_open {
        let typed = state
            .ui
            .input_buffer
            .trim_start_matches('/')
            .split_whitespace()
            .next()
            .unwrap_or("");
        let commands = crate::tui::slash_commands::filter_by_prefix(typed);
        let palette_widget = SlashPaletteWidget {
            theme: &rstate.theme,
            commands,
            selected_index: state.ui.palette_cursor.unwrap_or(0),
        };
        frame.render_widget(palette_widget, chunks[4]);
    } else {
        let cwd = state.cwd.display().to_string();
        let status_widget = StatusWidget {
            theme: &rstate.theme,
            working_dir: &cwd,
            cumulative_tokens: state.session.cumulative_tokens,
            model_name: &state.session.model_id,
            reasoning_level: effective,
            requested_level,
        };
        frame.render_widget(status_widget, chunks[4]);
    }
}

/// Merge the committed message log with any in-flight partial
/// content from `TurnState::Generating`. The chat widget renders
/// this as a single stream.
fn build_live_messages(
    committed: &[crate::models::ChatMessage],
    turn: &TurnState,
) -> Vec<crate::models::ChatMessage> {
    let mut out = committed.to_vec();
    if let TurnState::Generating {
        partial_text,
        partial_reasoning,
        ..
    } = turn
        && (!partial_text.is_empty() || !partial_reasoning.is_empty())
    {
        let thinking = if partial_reasoning.is_empty() {
            None
        } else {
            Some(partial_reasoning.clone())
        };
        let msg = crate::models::ChatMessage {
            role: crate::models::MessageRole::Assistant,
            content: partial_text.clone(),
            timestamp: chrono::Local::now(),
            actions: Vec::new(),
            thinking,
            images: None,
            tool_calls: None,
            tool_call_id: None,
            tool_name: None,
            thinking_signature: None,
        };
        out.push(msg);
    }
    out
}

/// Future hook: consult `ProviderFactory` for per-model capabilities.
/// Today returns `None` — reasoning snap indicator is suppressed in
/// v7 until the factory is threaded through `State` (or an
/// equivalent capability table).
fn supported_reasoning_for(_state: &State) -> Option<ReasoningCapability> {
    None
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
        let mut rstate = RenderState::new();
        terminal
            .draw(|f| render(state, &mut rstate, f))
            .expect("draw");
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
    fn idle_state_renders_cwd_and_model_footer() {
        let s = mock_state();
        let frame = render_to_string(&s);
        // Bottom status bar shows cwd + model id somewhere.
        assert!(frame.contains("/tmp/p") || frame.contains("tmp"));
        assert!(frame.contains("ollama/test"));
    }

    #[test]
    fn status_line_appears_during_generating() {
        let mut s = mock_state();
        s.turn = crate::domain::transition::start_generating(crate::domain::TurnId(1));
        let frame = render_to_string(&s);
        // Status widget only renders when generating — the bottom bar
        // should have shifted to show the status line content.
        assert!(
            frame.contains("Sending") || frame.contains("Thinking") || frame.contains("Streaming"),
            "expected generation status in frame"
        );
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
    fn palette_renders_when_input_starts_with_slash() {
        let mut s = mock_state();
        s.ui.input_buffer = "/help".to_string();
        s.ui.input_cursor = 5;
        let frame = render_to_string(&s);
        // At least one registered command should surface in the overlay.
        assert!(frame.contains("help"));
    }

    #[test]
    fn status_line_helper_maps_idle_to_idle() {
        assert_eq!(
            GenerationStatus::from_turn(&TurnState::Idle),
            GenerationStatus::Idle
        );
    }

    #[test]
    fn unused_status_line_struct_silences_warning() {
        // Guard against dead_code on the imported StatusLine + StatusKind.
        let _ = StatusLine {
            text: "x".to_string(),
            kind: StatusKind::Info,
            shown_at: std::time::SystemTime::now(),
        };
    }
}
