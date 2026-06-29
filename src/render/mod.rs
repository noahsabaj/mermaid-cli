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
    SlashPaletteWidget, StatusWidget, build_status_lines,
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
    /// Host + user for the status bar's `user@host:cwd` line, read once at
    /// startup so `StatusWidget::render` doesn't hit the environment on every
    /// frame (#55). Process-constant, so caching here is exact.
    pub hostname: String,
    pub username: String,
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
            hostname: std::env::var("HOSTNAME")
                .or_else(|_| std::env::var("HOST"))
                .unwrap_or_else(|_| "localhost".to_string()),
            username: std::env::var("USER")
                .or_else(|_| std::env::var("USERNAME"))
                .unwrap_or_else(|_| "user".to_string()),
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

    // Build the status-line rows up front (wrapped to the terminal width) so
    // the layout reserves exactly the height they need — a long
    // `Running tools: <cmd>` plus the trailing `(esc to interrupt …)` fold onto
    // continuation rows instead of bleeding off the right edge.
    let status_lines = if state.is_busy() {
        // Elapsed is computed from the injected `state.now` (stamped every tick),
        // not the live wall clock, so the rendered frame is a pure function of
        // State (Cause 3). Visually identical — both resolve to whole seconds.
        let now_sys = std::time::SystemTime::from(state.now);
        let elapsed_since =
            |t: std::time::SystemTime| now_sys.duration_since(t).map(|d| d.as_secs()).unwrap_or(0);
        let elapsed_secs = match &state.turn {
            TurnState::Generating { started, .. }
            | TurnState::Compacting { started, .. }
            | TurnState::ExecutingTools { started, .. } => elapsed_since(*started),
            TurnState::Cancelling { since, .. } => elapsed_since(*since),
            TurnState::Idle => 0,
        };
        // While tools run, name the in-flight one(s) so the status line isn't an
        // opaque "Running tools…". In-flight = the call slots without an outcome.
        let active_tool = match &state.turn {
            TurnState::ExecutingTools {
                calls, outcomes, ..
            } => {
                let pending = outcomes.iter().filter(|o| o.is_none()).count();
                calls
                    .iter()
                    .zip(outcomes)
                    .find(|(_, o)| o.is_none())
                    .map(|(call, _)| {
                        let (action, target) = crate::domain::display_info_for(call);
                        let label = if target.is_empty() {
                            action
                        } else {
                            format!("{action} {target}")
                        };
                        if pending > 1 {
                            format!("{label} (+{} more)", pending - 1)
                        } else {
                            label
                        }
                    })
            },
            _ => None,
        };
        // Live, char-based estimate of tokens generated so far (answer + thinking).
        // Marked estimated (`~`) — the authoritative count lands in the footer
        // once the turn's `Done` usage arrives.
        let (tokens_display, tokens_estimated) = match &state.turn {
            TurnState::Generating { tokens, .. } => (*tokens, true),
            _ => (0, false),
        };
        build_status_lines(
            GenerationStatus::from_turn(&state.turn),
            elapsed_secs,
            tokens_display,
            tokens_estimated,
            active_tool.as_deref(),
            &state.ui.queued_messages,
            &rstate.theme,
            // Match the 1-cell horizontal pad the status zone is rendered with.
            frame.area().width.saturating_sub(2),
        )
    } else {
        Vec::new()
    };

    let attachment_height = if state.ui.attachments.is_empty() {
        0
    } else {
        1
    };

    // The transient status banner that used to live here is gone — that zone is
    // the generation spinner's alone. Feedback, errors, and command results now
    // post into the chat transcript instead. The zone is kept at height 0 to
    // preserve the chunk indices below.
    let status_banner_height: u16 = 0;

    // Reserve the status zone's height to match its row count, but never so much
    // that the input box or bottom bar get evicted on a short terminal: keep room
    // for the chat floor (Min 10), the input box, the bottom bar (≥2), and the
    // banner/attachment rows. (The trailing Length zones would otherwise starve
    // before the Min(10) chat zone does.)
    let status_reserve = 10 + input_height + 2 + status_banner_height + attachment_height;
    let status_line_height = (status_lines.len() as u16)
        .min(6)
        .min(frame.area().height.saturating_sub(status_reserve));

    // Bottom region: one of three widgets based on UI mode.
    //   - ConversationList picker: 12-line pane.
    //   - Slash palette (input starts with `/`): 3–10 lines based on
    //     filter match count.
    //   - Otherwise: 2-line status bar.
    // Precedence: approval modal > confirm modal > ConversationList picker >
    // slash palette > status bar. Approvals/confirms are interrupts that
    // overlay regardless of input mode.
    let approval_item = state.pending_approval.front();
    let confirm_open = approval_item.is_none() && state.confirm.is_some();
    let conv_list_open = approval_item.is_none()
        && !confirm_open
        && matches!(
            state.ui.mode,
            crate::domain::UiMode::ConversationList { .. }
        );
    let palette_open = approval_item.is_none()
        && !confirm_open
        && !conv_list_open
        && state.ui.input_buffer.starts_with('/');
    let bottom_height = if let Some(item) = approval_item {
        // border(2) + body lines + blank(1) + 3 option lines
        let body_lines = item.prompt.lines().count().clamp(1, 6) as u16;
        2 + body_lines + 1 + 3
    } else if confirm_open {
        6
    } else if conv_list_open {
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
        show_reasoning: state.ui.show_reasoning,
    };
    frame.render_stateful_widget(chat_widget, chat_area, &mut rstate.chat);

    // Status line for every active turn (built above, already fit to width).
    // Indented 1 cell to align with the chat column's 1-cell pad.
    if !status_lines.is_empty() {
        let status_area = chunks[1].inner(Margin {
            horizontal: 1,
            vertical: 0,
        });
        frame.render_widget(ratatui::widgets::Paragraph::new(status_lines), status_area);
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

    // (Status-banner zone intentionally left unpainted — see status_banner_height.)

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
    if let Some(item) = state.pending_approval.front() {
        use widgets::ApprovalModalWidget;
        // Content-bearing external tools (type_text, MCP, …) are
        // non-allowlistable: the gate leaves their scope empty, and we omit the
        // "don't ask again" option so the user can't blanket-approve them (#6, #31).
        let options = if item.allowlist_scope.is_empty() {
            vec!["1. Yes".to_string(), "2. No  (Esc)".to_string()]
        } else {
            vec![
                "1. Yes".to_string(),
                format!("2. Yes, and don't ask again for `{}`", item.allowlist_scope),
                "3. No  (Esc)".to_string(),
            ]
        };
        let widget = ApprovalModalWidget {
            theme: &rstate.theme,
            title: format!("Approval required — {}  [{}]", item.tool, item.risk),
            body: item.prompt.as_str(),
            options,
            selected_index: Some(item.selected_option),
            accent: rstate.theme.colors.warning.to_color(),
        };
        frame.render_widget(widget, chunks[5]);
    } else if let Some(confirm) = &state.confirm {
        use widgets::ApprovalModalWidget;
        let widget = ApprovalModalWidget {
            theme: &rstate.theme,
            title: "Confirm".to_string(),
            body: confirm.prompt.as_str(),
            options: vec!["y. Yes".to_string(), "n. No  (Esc)".to_string()],
            selected_index: None,
            accent: rstate.theme.colors.warning.to_color(),
        };
        frame.render_widget(widget, chunks[5]);
    } else if let crate::domain::UiMode::ConversationList { candidates, cursor } = &state.ui.mode {
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
            hostname: &rstate.hostname,
            username: &rstate.username,
            context_usage: state.session.context_usage.as_ref(),
            last_usage: state.session.last_token_usage,
            session_usage: state.session.cumulative_token_usage,
            model_name: &state.session.model_id,
            reasoning_level: effective,
            requested_level,
            safety_mode: state.session.safety_mode,
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
        s.turn = crate::domain::transition::start_generating(
            crate::domain::TurnId(1),
            std::time::SystemTime::now(),
        );
        let frame = render_to_string(&s);
        assert!(
            frame.contains("Sending") || frame.contains("Thinking") || frame.contains("Streaming"),
            "expected generation status in frame"
        );
    }

    #[test]
    fn status_line_names_the_in_flight_tool() {
        use crate::domain::PendingToolCall;
        use crate::models::tool_call::{FunctionCall, ToolCall as ModelToolCall};
        let mut s = mock_state();
        let call = PendingToolCall {
            call_id: crate::domain::ToolCallId(1),
            source: ModelToolCall {
                id: Some("c1".to_string()),
                function: FunctionCall {
                    name: "execute_command".to_string(),
                    arguments: serde_json::json!({"command": "npm run dev"}),
                },
            },
        };
        s.turn = TurnState::ExecutingTools {
            id: crate::domain::TurnId(1),
            started: std::time::SystemTime::now(),
            calls: vec![call],
            outcomes: vec![None],
        };
        let frame = render_to_string(&s);
        assert!(frame.contains("Running tools"), "got: {frame}");
        assert!(
            frame.contains("npm run dev"),
            "status line must name the in-flight command; got: {frame}"
        );
    }

    #[test]
    fn status_line_appears_during_tool_execution_and_shows_queue() {
        let mut s = mock_state();
        s.turn = TurnState::ExecutingTools {
            id: crate::domain::TurnId(1),
            started: std::time::SystemTime::now(),
            calls: Vec::new(),
            outcomes: Vec::new(),
        };
        s.ui.queued_messages
            .push_back(crate::domain::QueuedMessage {
                text: "please steer this".to_string(),
                attachment_ids: Vec::new(),
            });
        let frame = render_to_string(&s);
        assert!(frame.contains("Running tools"), "expected tool status");
        assert!(
            frame.contains("please steer this"),
            "queued busy input must be visible"
        );
    }

    #[test]
    fn reasoning_blocks_are_collapsed_by_default() {
        let mut s = mock_state();
        let mut first_msg = crate::models::ChatMessage::assistant("first visible answer");
        first_msg.thinking = Some("first private chain of thought".to_string());
        s.session.append(first_msg);
        let mut second_msg = crate::models::ChatMessage::assistant("second visible answer");
        second_msg.thinking = Some("second private chain of thought".to_string());
        s.session.append(second_msg);
        let frame = render_to_string(&s);
        assert_eq!(frame.matches("Reasoning hidden").count(), 1);
        assert!(frame.contains("first visible answer"));
        assert!(frame.contains("second visible answer"));
        assert!(!frame.contains("first private chain of thought"));
        assert!(!frame.contains("second private chain of thought"));
    }

    /// Regression: a "thought, then immediately called a tool" turn (hidden
    /// reasoning + empty text + actions) must still put one blank line
    /// between the "Reasoning hidden" placeholder and the first action —
    /// the same gap every other block pair gets. Previously the placeholder
    /// rendered flush against the first "● Bash(…)" line.
    #[test]
    fn reasoning_placeholder_is_gapped_from_following_action() {
        let mut s = mock_state();
        let mut msg = crate::models::ChatMessage::assistant("");
        msg.thinking = Some("private chain of thought".to_string());
        msg.actions.push(crate::domain::ActionDisplay {
            action_type: "Bash".to_string(),
            target: "dir".to_string(),
            result: crate::domain::ActionResult::Success {
                output: "ok".to_string(),
                images: None,
            },
            details: crate::domain::ActionDetails::Simple,
            duration_seconds: Some(0.015),
            metadata: None,
        });
        s.session.append(msg);
        let frame = render_to_string(&s);
        let lines: Vec<&str> = frame.lines().collect();
        let idx = lines
            .iter()
            .position(|l| l.contains("Reasoning hidden"))
            .expect("placeholder must render");
        assert!(
            lines[idx + 1].trim().is_empty(),
            "a blank line must separate the placeholder from the action; got {:?}",
            &lines[idx..=(idx + 2).min(lines.len() - 1)]
        );
        assert!(
            lines[idx..].iter().any(|l| l.contains("Bash")),
            "the action must still render after the placeholder"
        );
    }

    #[test]
    fn scrollbar_shows_when_chat_overflows() {
        let mut s = mock_state();
        for i in 0..60 {
            s.session
                .append(crate::models::ChatMessage::assistant(format!("msg {i}")));
        }
        let frame = render_to_string(&s);
        // ratatui's Scrollbar renders a "█" thumb in the reserved gutter once
        // the transcript is taller than the viewport.
        assert!(
            frame.contains('█'),
            "a scrollbar thumb should render when the transcript overflows the viewport"
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

    /// The transient status banner was removed — that zone above the input
    /// belongs to the generation spinner alone, so `state.status` is never
    /// painted even when set. Command feedback goes to the chat transcript now.
    #[test]
    fn state_status_is_never_painted() {
        let mut s = mock_state();
        s.status = Some(StatusLine {
            text: "Reasoning: high".to_string(),
            kind: StatusKind::Info,
            shown_at: std::time::SystemTime::now(),
        });
        let frame = render_to_string(&s);
        assert!(
            !frame.contains("Reasoning: high"),
            "the status banner is gone; state.status must not reach the screen"
        );
    }
}
