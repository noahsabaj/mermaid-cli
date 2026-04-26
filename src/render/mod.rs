//! Pure view: `fn render(&State, &mut RenderCache, &mut Frame)`.
//!
//! Three contracts:
//!   1. Never mutates `State`. The view is fully derived.
//!   2. Never performs I/O. All state — model lists, MCP status,
//!      file contents — is whatever the reducer put in `State`.
//!   3. Never holds a `&mut App` / `&mut anything` other than the
//!      `Frame` ratatui owns and the render-layer `RenderCache`
//!      (which is memoization + scroll-position bookkeeping, not
//!      reducer state).
//!
//! Signature: `fn render(&State, &mut RenderCache, &mut Frame)`.
//! The `&mut RenderCache` is memoization only (markdown parse
//! cache, scroll position, theme choice) — it never affects
//! reducer outcomes or persisted state.

pub mod diff;
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
/// reducer state. Owned by `app::run_interactive`; passed as `&mut`
/// to `render()` per frame.
///
/// Contents are pure memoization + UI affordances (scroll position,
/// markdown cache, theme choice). Nothing here affects what the
/// reducer sees or what ends up on disk — the cache can be dropped
/// and rebuilt from `&State` at any time.
pub struct RenderCache {
    pub chat: ChatState,
    pub markdown_cache: FxHashMap<u64, Vec<ratatui::text::Line<'static>>>,
    pub theme: theme::Theme,
    /// F13: last `state.ui.mouse_scroll_accum` value we applied to
    /// `chat.scroll_up/down`. Diffing lets the reducer stay pure —
    /// it just publishes a counter; render owns the chat-state side.
    last_mouse_scroll_accum: i32,
}

impl Default for RenderCache {
    fn default() -> Self {
        Self {
            chat: ChatState::new(),
            markdown_cache: FxHashMap::default(),
            theme: theme::Theme::dark(),
            last_mouse_scroll_accum: 0,
        }
    }
}

impl RenderCache {
    pub fn new() -> Self {
        Self::default()
    }
}

/// The entrypoint. Call once per render pass from the main loop.
pub fn render(state: &State, rstate: &mut RenderCache, frame: &mut Frame) {
    // F13: consume any pending mouse-scroll accumulator. The reducer
    // publishes a monotonic counter on `ui.mouse_scroll_accum`; we
    // apply the delta to `ChatState` since the reducer isn't allowed
    // to touch render-layer state directly.
    let pending = state.ui.mouse_scroll_accum - rstate.last_mouse_scroll_accum;
    if pending > 0 {
        rstate.chat.scroll_up(pending as u16);
    } else if pending < 0 {
        rstate.chat.scroll_down((-pending) as u16);
    }
    rstate.last_mouse_scroll_accum = state.ui.mouse_scroll_accum;

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

    // F9: one-row banner for `state.status`. Previously the reducer
    // set state.status for slash commands, MCP errors, and model-pull
    // progress but no widget painted it. Height is 1 when a status is
    // present, 0 otherwise.
    let status_banner_height: u16 = if state.status.is_some() { 1 } else { 0 };

    // Bottom region: one of three widgets based on UI mode.
    //   - ConversationList picker: 12-line pane.
    //   - Slash palette (input starts with `/`): 3–10 lines based on
    //     filter match count.
    //   - Otherwise: 2-line status bar.
    let conv_list_open = matches!(
        state.ui.mode,
        crate::domain::UiMode::ConversationList { .. }
    );
    let palette_open = !conv_list_open && state.ui.input_buffer.starts_with('/');
    let bottom_height = if conv_list_open {
        12
    } else if palette_open {
        let typed = state
            .ui
            .input_buffer
            .trim_start_matches('/')
            .split_whitespace()
            .next()
            .unwrap_or("");
        let row_count = crate::domain::slash_commands::filter_by_prefix(typed)
            .len()
            .clamp(1, 8);
        (row_count as u16) + 2
    } else {
        2
    };

    // 6-zone vertical layout: chat / status line / attachments /
    // status banner / input / bottom. The banner sits directly above
    // input so the eye finds "what just happened" right next to
    // "what's next to type".
    use ratatui::layout::{Constraint, Direction, Layout};
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(10),
            Constraint::Length(status_line_height),
            Constraint::Length(attachment_height),
            Constraint::Length(status_banner_height),
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

    // F9 banner for state.status — above input, below attachments.
    if let Some(ref status) = state.status {
        let banner = widgets::StatusBannerWidget {
            theme: &rstate.theme,
            status,
        };
        frame.render_widget(banner, chunks[3]);
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
    frame.render_stateful_widget(input_widget, chunks[4], &mut input_widget_state);

    // Cursor visible unless focus is on attachments.
    if !state.ui.attachment_focused {
        let input_area = chunks[4];
        let content_width = input_area.width.saturating_sub(2) as usize;
        let (cursor_row, cursor_col) = InputState::calculate_cursor_position(
            &state.ui.input_buffer,
            state.ui.input_cursor.min(state.ui.input_buffer.len()),
            content_width,
        );
        frame.set_cursor_position((input_area.x + cursor_col + 2, input_area.y + 1 + cursor_row));
    }

    // Effective reasoning level. Per-model supported_reasoning cap
    // isn't threaded through `State` yet; defaults to no snap
    // indicator until `ProviderFactory::capabilities` reaches here.
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

    // Bottom: conversation-list picker, slash-palette overlay, or
    // persistent status bar — whichever the UI mode dictates.
    if let crate::domain::UiMode::ConversationList { candidates, cursor } = &state.ui.mode {
        use widgets::ConversationListWidget;
        let widget = ConversationListWidget {
            theme: &rstate.theme,
            candidates,
            cursor: *cursor,
        };
        frame.render_widget(widget, chunks[5]);
    } else if palette_open {
        let typed = state
            .ui
            .input_buffer
            .trim_start_matches('/')
            .split_whitespace()
            .next()
            .unwrap_or("");
        let commands = crate::domain::slash_commands::filter_by_prefix(typed);
        let palette_widget = SlashPaletteWidget {
            theme: &rstate.theme,
            commands,
            selected_index: state.ui.palette_cursor.unwrap_or(0),
        };
        frame.render_widget(palette_widget, chunks[5]);
    } else {
        let cwd = state.cwd.display().to_string();
        let status_widget = StatusWidget {
            theme: &rstate.theme,
            working_dir: &cwd,
            context_usage: state.session.context_usage.as_ref(),
            last_usage: state.session.last_token_usage,
            session_usage: state.session.cumulative_token_usage,
            model_name: &state.session.model_id,
            reasoning_level: effective,
            requested_level,
        };
        frame.render_widget(status_widget, chunks[5]);
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
            kind: crate::models::ChatMessageKind::Normal,
            metadata: None,
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
/// Today returns `None` — reasoning snap indicator is suppressed
/// until the factory is threaded through `State` (or an equivalent
/// capability table).
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
        let mut rstate = RenderCache::new();
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

    /// F9: `state.status` is painted as a banner above input. Before
    /// this, the reducer would set `state.status` for slash-command
    /// feedback and MCP errors but no widget displayed it.
    #[test]
    fn state_status_renders_as_banner() {
        let mut s = mock_state();
        s.status = Some(StatusLine {
            text: "Reasoning: high".to_string(),
            kind: StatusKind::Info,
            shown_at: std::time::SystemTime::now(),
        });
        let frame = render_to_string(&s);
        assert!(
            frame.contains("Reasoning: high"),
            "state.status must reach the screen"
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
