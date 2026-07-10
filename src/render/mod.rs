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
pub mod markdown;
pub mod theme;
pub mod widgets;

use ratatui::{Frame, layout::Margin};
use rustc_hash::FxHashMap;
use unicode_width::UnicodeWidthChar;

use crate::domain::{State, TurnState};
use crate::models::{ReasoningCapability, ReasoningLevel, nearest_effort};

use widgets::{
    ChatState, ChatWidget, GenerationStatus, InputState, InputWidget, SlashPaletteWidget,
    StatusWidget, build_status_lines,
};

/// Transient render-layer state that lives across frames but isn't
/// reducer state. Owned by `app::run_interactive`; passed as `&mut`
/// to `render()` per frame.
///
/// Contents are pure memoization + UI affordances (scroll position,
/// wrapped-line cache, theme choice). Nothing here affects what the
/// reducer sees or what ends up on disk — the cache can be dropped
/// and rebuilt from `&State` at any time.
pub struct RenderCache {
    pub chat: ChatState,
    /// Per-message render cache: `(content, theme, width)` hash → fully wrapped,
    /// role-prefixed assistant lines, so committed messages aren't re-parsed or
    /// re-wrapped every frame (#134).
    pub wrapped_line_cache: FxHashMap<u64, Vec<ratatui::text::Line<'static>>>,
    /// Memoized stitched transcript: committed `Continuation` messages folded
    /// into their predecessor bubble and spent `RecoveryNudge` notes hidden.
    /// Rebuilt only when the committed log changes (keyed by a content
    /// fingerprint) — without the memo, every idle frame after the first
    /// auto-continue would deep-clone the whole transcript forever.
    stitched: Option<StitchedMemo>,
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
    /// Last `state.ui.scroll_to_bottom_seq` we acted on; a bump (keyboard
    /// `End`) means resume auto-follow / jump to the newest message.
    last_scroll_to_bottom_seq: u32,
}

impl Default for RenderCache {
    fn default() -> Self {
        Self {
            chat: ChatState::new(),
            wrapped_line_cache: FxHashMap::default(),
            theme: theme::Theme::dark(),
            hostname: std::env::var("HOSTNAME")
                .or_else(|_| std::env::var("HOST"))
                .unwrap_or_else(|_| "localhost".to_string()),
            username: std::env::var("USER")
                .or_else(|_| std::env::var("USERNAME"))
                .unwrap_or_else(|_| "user".to_string()),
            stitched: None,
            last_mouse_scroll_accum: 0,
            last_scroll_to_bottom_seq: 0,
        }
    }
}

/// See [`RenderCache::stitched`].
struct StitchedMemo {
    key: u64,
    messages: Vec<crate::models::ChatMessage>,
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
    // Keyboard End: a bumped counter means jump back to the newest message.
    if state.ui.scroll_to_bottom_seq != rstate.last_scroll_to_bottom_seq {
        rstate.chat.resume_auto_scroll();
        rstate.last_scroll_to_bottom_seq = state.ui.scroll_to_bottom_seq;
    }

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
            // A model run (generating + executing tools) anchors to the run start
            // so the timer spans the whole agentic loop, not just this step.
            TurnState::Generating { started, .. } | TurnState::ExecutingTools { started, .. } => {
                state
                    .runtime
                    .run_started
                    .map_or_else(|| elapsed_since(*started), elapsed_since)
            },
            TurnState::Compacting { started, .. } => elapsed_since(*started),
            TurnState::Cancelling { since, .. } => elapsed_since(*since),
            TurnState::Idle => 0,
        };
        let active_tool = active_tool_label(state);
        // Live, char-based estimate of tokens generated so far this run (answer +
        // thinking), accumulated across tool steps via `run_committed_tokens` so it
        // doesn't reset each model call. Marked estimated (`~`); the authoritative
        // count lands in the footer once the turn's `Done` usage arrives.
        let committed = state.runtime.run_committed_tokens;
        let (tokens_display, tokens_estimated) = match &state.turn {
            TurnState::Generating { tokens, .. } => (committed + *tokens, true),
            TurnState::ExecutingTools { .. } => (committed, true),
            _ => (0, false),
        };
        build_status_lines(
            GenerationStatus::from_turn(&state.turn),
            elapsed_secs,
            tokens_display,
            tokens_estimated,
            active_tool.as_deref(),
            &state.ui.queued_messages,
            exit_armed(state),
            &rstate.theme,
            // Match the 1-cell horizontal pad the status zone is rendered with.
            frame.area().width.saturating_sub(2),
        )
    } else {
        Vec::new()
    };

    // Reserve the status zone's height to match its row count, but never so much
    // that the input box or bottom bar get evicted on a short terminal: keep room
    // for the chat floor (Min 10), the input box, and the bottom bar (≥2). (The
    // trailing Length zones would otherwise starve before the Min(10) chat zone.)
    let status_reserve = 10 + input_height + 2;
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
    let question_item = if approval_item.is_none() {
        state.pending_question.front()
    } else {
        None
    };
    let confirm_open =
        approval_item.is_none() && question_item.is_none() && state.confirm.is_some();
    let conv_list_open = approval_item.is_none()
        && question_item.is_none()
        && !confirm_open
        && matches!(
            state.ui.mode,
            crate::domain::UiMode::ConversationList { .. }
        );
    let file_picker_open = approval_item.is_none()
        && question_item.is_none()
        && !confirm_open
        && !conv_list_open
        && state.ui.file_picker_open();
    let palette_open = approval_item.is_none()
        && question_item.is_none()
        && !confirm_open
        && !conv_list_open
        && !file_picker_open
        && state.ui.input_buffer.starts_with('/');
    let bottom_height = if let Some(item) = approval_item {
        // border(2) + body lines + blank(1) + 3 option lines
        let body_lines = item.prompt.lines().count().clamp(1, 6) as u16;
        2 + body_lines + 1 + 3
    } else if let Some(qset) = question_item {
        widgets::question_modal_height(qset, &rstate.theme)
    } else if confirm_open {
        6
    } else if conv_list_open {
        12
    } else if file_picker_open {
        let rows = state.ui.file_picker_matches.len().clamp(1, 8);
        (rows as u16) + 2
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

    // 4-zone vertical layout: chat / status line / input / bottom. Pasted images
    // are inline `[Image #N]` tokens in the input now, so there's no separate
    // attachment zone.
    use ratatui::layout::{Constraint, Direction, Layout};
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(10),
            Constraint::Length(status_line_height),
            Constraint::Length(input_height),
            Constraint::Length(bottom_height),
        ])
        .split(frame.area());

    // Chat area with 1-cell horizontal padding.
    let chat_area = chunks[0].inner(Margin {
        horizontal: 1,
        vertical: 0,
    });
    // Stitch pre-pass: fold auto-continued replies into one bubble and hide
    // spent recovery nudges. Sessions without either kind skip this entirely
    // (borrowed slice, no fingerprint); with them, the memo makes idle frames
    // a hash-check instead of a transcript clone.
    let committed = state.session.messages();
    let base: &[crate::models::ChatMessage] = if needs_stitch(committed) {
        let key = stitch_fingerprint(committed);
        if rstate.stitched.as_ref().map(|m| m.key) != Some(key) {
            rstate.stitched = Some(StitchedMemo {
                key,
                messages: stitch_committed(committed),
            });
        }
        &rstate
            .stitched
            .as_ref()
            .expect("stitched memo populated above")
            .messages
    } else {
        committed
    };
    let live_messages = build_live_messages(base, &state.turn, state.now);
    let chat_widget = ChatWidget {
        messages: live_messages.as_ref(),
        theme: &rstate.theme,
        wrapped_line_cache: &mut rstate.wrapped_line_cache,
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

    // Input box (chunks[2] now that the attachment zone is gone).
    let input_widget = InputWidget {
        input: state.ui.input_buffer.as_str(),
        showing_command_hints: state.ui.input_buffer.starts_with('/'),
        theme: &rstate.theme,
        reasoning_active: state.session.reasoning != ReasoningLevel::None,
        exit_armed: exit_armed(state),
    };
    let mut input_widget_state = InputState {
        cursor_position: state.ui.input_cursor.min(state.ui.input_buffer.len()),
    };
    frame.render_stateful_widget(input_widget, chunks[2], &mut input_widget_state);

    // Cursor always tracks the input caret.
    let input_area = chunks[2];
    let content_width = input_area.width.saturating_sub(2) as usize;
    let (cursor_row, cursor_col) = InputState::calculate_cursor_position(
        &state.ui.input_buffer,
        state.ui.input_cursor.min(state.ui.input_buffer.len()),
        content_width,
    );
    frame.set_cursor_position((input_area.x + cursor_col + 2, input_area.y + 1 + cursor_row));

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
        frame.render_widget(widget, chunks[3]);
    } else if let Some(qset) = state.pending_question.front() {
        use widgets::QuestionModalWidget;
        let widget = QuestionModalWidget {
            theme: &rstate.theme,
            set: qset,
        };
        frame.render_widget(widget, chunks[3]);
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
        frame.render_widget(widget, chunks[3]);
    } else if let crate::domain::UiMode::ConversationList { candidates, cursor } = &state.ui.mode {
        use widgets::ConversationListWidget;
        let widget = ConversationListWidget {
            theme: &rstate.theme,
            candidates,
            cursor: *cursor,
        };
        frame.render_widget(widget, chunks[3]);
    } else if file_picker_open {
        use widgets::FilePickerWidget;
        let widget = FilePickerWidget {
            theme: &rstate.theme,
            matches: &state.ui.file_picker_matches,
            selected_index: state.ui.file_picker_cursor.unwrap_or(0),
            loading: state.ui.project_files_loading && state.ui.project_files.is_none(),
        };
        frame.render_widget(widget, chunks[3]);
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
        frame.render_widget(palette_widget, chunks[3]);
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
        frame.render_widget(status_widget, chunks[3]);
    }
}

/// Can a `Continuation` message be folded into this predecessor? Guards the
/// stitch against non-bubble assistants: a compaction checkpoint's assistant
/// half (`ContextCheckpoint`, rendered as an event block), the empty
/// error-carrier message, or an assistant that ended in tool calls.
/// `pub(crate)` so the chat widget applies the same rule when deciding to
/// draw a streaming continuation without a fresh bubble prefix.
pub(crate) fn mergeable_into(prev: &crate::models::ChatMessage) -> bool {
    prev.role == crate::models::MessageRole::Assistant
        && matches!(
            prev.kind,
            crate::models::ChatMessageKind::Normal | crate::models::ChatMessageKind::Continuation
        )
        && prev.tool_calls.is_none()
}

/// Does the committed log contain anything the stitch pre-pass would change?
/// The common session never does — and then rendering keeps borrowing the
/// committed slice with zero copies, exactly as before auto-continue existed.
fn needs_stitch(committed: &[crate::models::ChatMessage]) -> bool {
    committed.iter().any(|m| {
        matches!(
            m.kind,
            crate::models::ChatMessageKind::Continuation
                | crate::models::ChatMessageKind::RecoveryNudge
        )
    })
}

/// Fingerprint of every committed-message field the stitched transcript
/// depends on. Cheap relative to re-stitching (hashing, no cloning); mirrors
/// the chat widget's frame fingerprint so in-place mutations that don't
/// change message count (e.g. an action attached to the last message during a
/// tool run) still invalidate the memo.
fn stitch_fingerprint(committed: &[crate::models::ChatMessage]) -> u64 {
    use std::fmt::Write as _;
    use std::hash::{Hash, Hasher};
    struct HashWrite<'a, H: Hasher>(&'a mut H);
    impl<H: Hasher> std::fmt::Write for HashWrite<'_, H> {
        fn write_str(&mut self, s: &str) -> std::fmt::Result {
            self.0.write(s.as_bytes());
            Ok(())
        }
    }
    let mut h = rustc_hash::FxHasher::default();
    committed.len().hash(&mut h);
    for msg in committed {
        msg.content.hash(&mut h);
        msg.thinking.hash(&mut h);
        msg.timestamp.timestamp().hash(&mut h);
        msg.images.as_ref().map_or(0, |v| v.len()).hash(&mut h);
        msg.image_numbers
            .as_ref()
            .map_or(0, |v| v.len())
            .hash(&mut h);
        let mut hw = HashWrite(&mut h);
        let _ = write!(
            hw,
            "{:?}|{:?}|{:?}|{:?}",
            msg.role,
            msg.kind,
            msg.actions,
            msg.tool_calls.as_ref().map(|t| t.len())
        );
    }
    h.finish()
}

/// The display stitch: fold committed `Continuation` messages into their
/// predecessor bubble and hide spent `RecoveryNudge` notes, so an
/// auto-continued reply reads as ONE uninterrupted assistant message.
///
/// Display-only — canonical history keeps the separate messages exactly as
/// they crossed the wire (provider-correct, thinking-signature-safe). Merging
/// the contents into one string here also means one `parse_markdown` call, so
/// a code fence cut open by the output cap and re-closed in the continuation
/// renders as a single intact block. A `Continuation` whose predecessor is
/// not a mergeable bubble (archived by compaction, wedged system note)
/// renders as its own message — a graceful seam, never a wrong merge.
fn stitch_committed(committed: &[crate::models::ChatMessage]) -> Vec<crate::models::ChatMessage> {
    let mut out: Vec<crate::models::ChatMessage> = Vec::with_capacity(committed.len());
    for msg in committed {
        if msg.kind == crate::models::ChatMessageKind::RecoveryNudge {
            continue;
        }
        if msg.kind == crate::models::ChatMessageKind::Continuation
            && let Some(prev) = out.last_mut()
            && mergeable_into(prev)
        {
            merge_continuation(prev, msg);
            continue;
        }
        out.push(msg.clone());
    }
    out
}

/// Fold one continuation segment into the bubble it resumes. The seam gets a
/// conservative overlap trim (see `continuation_overlap`): a resume-echo of
/// the previous tail is dropped, anything ambiguous is kept.
fn merge_continuation(prev: &mut crate::models::ChatMessage, cont: &crate::models::ChatMessage) {
    let skip = crate::utils::continuation_overlap(&prev.content, &cont.content);
    prev.content.push_str(&cont.content[skip..]);
    if let Some(cont_thinking) = &cont.thinking {
        match &mut prev.thinking {
            Some(t) => {
                t.push_str("\n\n");
                t.push_str(cont_thinking);
            },
            None => prev.thinking = Some(cont_thinking.clone()),
        }
    }
    prev.actions.extend(cont.actions.iter().cloned());
    if let Some(imgs) = &cont.images {
        prev.images
            .get_or_insert_with(Vec::new)
            .extend(imgs.iter().cloned());
    }
    if let Some(nums) = &cont.image_numbers {
        prev.image_numbers
            .get_or_insert_with(Vec::new)
            .extend(nums.iter().copied());
    }
    // A continuation that resumed the reply and then called tools carries the
    // calls; the merged bubble inherits them (the guard ensured prev had none).
    if cont.tool_calls.is_some() {
        prev.tool_calls = cont.tool_calls.clone();
    }
}

/// Merge the committed message log with any in-flight partial
/// content from `TurnState::Generating`. The chat widget renders
/// this as a single stream.
///
/// `committed` is the (possibly stitched) display transcript. When the live
/// turn is an auto-continue, the pseudo-message is stamped `Continuation` so
/// the widget draws it as a prefix-less extension of the previous bubble, and
/// its leading resume-echo is trimmed against that bubble's tail — the
/// in-flight reply looks like one message while it streams, not just after
/// it commits.
fn build_live_messages<'a>(
    committed: &'a [crate::models::ChatMessage],
    turn: &TurnState,
    now: chrono::DateTime<chrono::Local>,
) -> std::borrow::Cow<'a, [crate::models::ChatMessage]> {
    // Idle / no-partial frames borrow the committed log directly — no per-frame
    // clone of the whole transcript. Only an in-flight partial forces an owned
    // copy (committed + the one live assistant message).
    if let TurnState::Generating {
        partial_text,
        partial_reasoning,
        continuation,
        ..
    } = turn
        && (!partial_text.is_empty() || !partial_reasoning.is_empty())
    {
        let thinking = if partial_reasoning.is_empty() {
            None
        } else {
            Some(partial_reasoning.clone())
        };
        let stitching = *continuation && committed.last().is_some_and(mergeable_into);
        let content = if stitching {
            let prev = &committed[committed.len() - 1].content;
            let skip = crate::utils::continuation_overlap(prev, partial_text);
            partial_text[skip..].to_string()
        } else {
            partial_text.clone()
        };
        let msg = crate::models::ChatMessage {
            role: crate::models::MessageRole::Assistant,
            content,
            // `state.now` (stamped each tick) keeps render a pure function of
            // State — never read the wall clock here.
            timestamp: now,
            kind: if stitching {
                crate::models::ChatMessageKind::Continuation
            } else {
                crate::models::ChatMessageKind::Normal
            },
            metadata: None,
            actions: Vec::new(),
            thinking,
            images: None,
            image_numbers: None,
            tool_calls: None,
            tool_call_id: None,
            tool_name: None,
            thinking_signature: None,
        };
        let mut out = committed.to_vec();
        out.push(msg);
        std::borrow::Cow::Owned(out)
    } else {
        std::borrow::Cow::Borrowed(committed)
    }
}

/// True while a first Ctrl+C's exit-confirmation window is open. Expiry is
/// lazy: compared against the injected `state.now` (stamped every tick), so
/// the hint disappears on the next tick frame with no reducer state change.
fn exit_armed(state: &State) -> bool {
    state
        .ui
        .exit_armed_until
        .is_some_and(|deadline| state.now <= deadline)
}

/// While tools run, name the first in-flight one so the status line isn't an
/// opaque "Running tools…". In-flight = the call slots without an outcome.
/// When the reducer holds live per-call activity (a subagent's current tool /
/// latest text, from `Msg::ToolProgress`), it is appended after the label so
/// a long-running child is never a silent spinner.
fn active_tool_label(state: &State) -> Option<String> {
    match &state.turn {
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
                    let mut label = if target.is_empty() {
                        action
                    } else {
                        format!("{action} {target}")
                    };
                    if let Some(live) = state.ui.live_tool_status.get(&call.call_id) {
                        label = format!("{label} · {live}");
                    }
                    if pending > 1 {
                        format!("{label} (+{} more)", pending - 1)
                    } else {
                        label
                    }
                })
        },
        _ => None,
    }
}

/// Future hook: consult `ProviderFactory` for per-model capabilities.
/// Today returns `None` — reasoning snap indicator is suppressed
/// until the factory is threaded through `State` (or an equivalent
/// capability table).
fn supported_reasoning_for(_state: &State) -> Option<ReasoningCapability> {
    None
}

/// Render one frame into a plain-text character grid at the given size.
/// Test-only: shared by the unit tests below and the snapshot suite
/// (`snapshots.rs`), which needs to control both the frame size and the
/// `RenderCache` (pinned hostname/username).
#[cfg(test)]
pub(crate) fn render_frame(
    state: &State,
    rstate: &mut RenderCache,
    width: u16,
    height: u16,
) -> String {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal.draw(|f| render(state, rstate, f)).expect("draw");
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

// TZ-sensitive (`temp_env` TZ pinning) and fixture scripts assume unix paths;
// the unit tests above already cover Windows-relevant logic.
#[cfg(all(test, unix))]
mod snapshots;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Config;
    use crate::domain::{State, TurnState};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;

    fn mock_state() -> State {
        State::new(
            Config::default(),
            PathBuf::from("/tmp/p"),
            "ollama/test".to_string(),
            chrono::Local::now(),
        )
    }

    fn render_to_string(state: &State) -> String {
        render_frame(state, &mut RenderCache::new(), 80, 24)
    }

    fn render_to_buffer(state: &State) -> ratatui::buffer::Buffer {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut rstate = RenderCache::new();
        terminal
            .draw(|f| render(state, &mut rstate, f))
            .expect("draw");
        terminal.backend().buffer().clone()
    }

    #[test]
    fn active_tool_label_appends_live_subagent_status() {
        use crate::domain::{PendingToolCall, ToolCallId, TurnId};

        let mut state = mock_state();
        let call_id = ToolCallId(7);
        state.turn = TurnState::ExecutingTools {
            id: TurnId(1),
            started: std::time::SystemTime::now(),
            calls: vec![PendingToolCall {
                call_id,
                source: crate::models::tool_call::ToolCall {
                    id: None,
                    function: crate::models::tool_call::FunctionCall {
                        name: "agent".to_string(),
                        arguments: serde_json::json!({"description": "explore crates"}),
                    },
                },
            }],
            outcomes: vec![None],
        };

        // Without live status: just the tool label.
        assert_eq!(
            active_tool_label(&state).as_deref(),
            Some("Agent explore crates"),
        );

        // With live status: the child's current activity rides along, so a
        // long-running subagent is never an opaque spinner.
        state
            .ui
            .live_tool_status
            .insert(call_id, "read_file…".to_string());
        assert_eq!(
            active_tool_label(&state).as_deref(),
            Some("Agent explore crates · read_file…"),
        );
    }

    #[test]
    fn build_live_messages_borrows_idle_and_stamps_partial_with_injected_now() {
        use crate::domain::{GenPhase, TurnId};
        use crate::models::ChatMessage;
        use std::borrow::Cow;
        use std::time::SystemTime;

        let committed = vec![ChatMessage::user("hi")];
        let now = chrono::Local::now();

        // Idle frames borrow the committed log unchanged — no per-frame clone.
        let idle = build_live_messages(&committed, &TurnState::Idle, now);
        assert!(matches!(idle, Cow::Borrowed(_)));
        assert_eq!(idle.len(), 1);

        // A generating partial yields an owned copy whose live message is stamped
        // from the injected `now`, never the wall clock (render purity, #135).
        let turn = TurnState::Generating {
            id: TurnId(1),
            started: SystemTime::now(),
            partial_text: "draft".to_string(),
            partial_reasoning: String::new(),
            tokens: 0,
            phase: GenPhase::Sending,
            thinking_signature: None,
            pending_tool_calls: Vec::new(),
            continuation: false,
        };
        let live = build_live_messages(&committed, &turn, now);
        assert!(matches!(live, Cow::Owned(_)));
        assert_eq!(live.len(), 2);
        assert_eq!(live[1].timestamp, now);
    }

    fn kinded(
        mut msg: crate::models::ChatMessage,
        kind: crate::models::ChatMessageKind,
    ) -> crate::models::ChatMessage {
        msg.kind = kind;
        msg
    }

    #[test]
    fn stitch_committed_merges_chain_and_hides_nudges() {
        use crate::models::{ChatMessage, ChatMessageKind};
        let mut part1 = ChatMessage::assistant("The audit found three issues in the resolver");
        part1.thinking = Some("first trace".to_string());
        // The continuation echoes the tail of part1 — the seam trim drops it.
        let mut part2 = kinded(
            ChatMessage::assistant("issues in the resolver, and here is the fix."),
            ChatMessageKind::Continuation,
        );
        part2.thinking = Some("second trace".to_string());
        let committed = vec![
            ChatMessage::user("audit the widget"),
            part1,
            kinded(
                ChatMessage::system("resume nudge"),
                ChatMessageKind::RecoveryNudge,
            ),
            part2,
        ];

        assert!(needs_stitch(&committed));
        let stitched = stitch_committed(&committed);
        assert_eq!(stitched.len(), 2, "user + one merged bubble");
        assert_eq!(
            stitched[1].content,
            "The audit found three issues in the resolver, and here is the fix.",
            "contents merge with the resume echo trimmed"
        );
        assert_eq!(
            stitched[1].thinking.as_deref(),
            Some("first trace\n\nsecond trace"),
            "both reasoning segments survive in order"
        );
        assert!(
            !stitched.iter().any(|m| m.content.contains("resume nudge")),
            "nudges never render"
        );
    }

    #[test]
    fn stitch_refuses_non_bubble_predecessor() {
        use crate::models::{ChatMessage, ChatMessageKind};
        // A continuation whose bubble was archived by compaction lands after
        // the checkpoint's assistant half — render it as its own message
        // (graceful seam) rather than merging into the event block.
        let committed = vec![
            kinded(
                ChatMessage::assistant("checkpoint summary"),
                ChatMessageKind::ContextCheckpoint,
            ),
            kinded(
                ChatMessage::assistant("orphaned continuation"),
                ChatMessageKind::Continuation,
            ),
        ];
        let stitched = stitch_committed(&committed);
        assert_eq!(stitched.len(), 2, "no merge into a checkpoint");
        assert_eq!(stitched[1].content, "orphaned continuation");
    }

    #[test]
    fn needs_stitch_is_false_for_plain_sessions() {
        use crate::models::ChatMessage;
        // The fast path: a session that never auto-continued skips the
        // pre-pass entirely (borrowed slice, no fingerprint, no clone).
        let committed = vec![
            ChatMessage::user("hi"),
            ChatMessage::assistant("hello"),
            ChatMessage::system("note"),
        ];
        assert!(!needs_stitch(&committed));
    }

    #[test]
    fn build_live_messages_stamps_streaming_continuation_and_trims_echo() {
        use crate::domain::{GenPhase, TurnId};
        use crate::models::{ChatMessage, ChatMessageKind};

        let committed = vec![ChatMessage::assistant(
            "the fix lands in the resolver module",
        )];
        let turn = TurnState::Generating {
            id: TurnId(2),
            started: std::time::SystemTime::now(),
            partial_text: "in the resolver module, specifically the clamp".to_string(),
            partial_reasoning: String::new(),
            tokens: 0,
            phase: GenPhase::Streaming,
            thinking_signature: None,
            pending_tool_calls: Vec::new(),
            continuation: true,
        };
        let live = build_live_messages(&committed, &turn, chrono::Local::now());
        let streamed = live.last().expect("pseudo-message appended");
        assert_eq!(
            streamed.kind,
            ChatMessageKind::Continuation,
            "the live half is stamped so the widget draws it prefix-less"
        );
        assert_eq!(
            streamed.content, ", specifically the clamp",
            "the leading resume echo is trimmed against the committed tail"
        );
    }

    #[test]
    fn auto_continued_reply_renders_as_one_bubble() {
        use crate::models::{ChatMessage, ChatMessageKind};
        let mut s = mock_state();
        s.session.append(ChatMessage::user("audit"), s.now);
        s.session
            .append(ChatMessage::assistant("part one of the reply"), s.now);
        s.session.append(
            kinded(
                ChatMessage::system("output limit — continuing"),
                ChatMessageKind::RecoveryNudge,
            ),
            s.now,
        );
        s.session.append(
            kinded(
                ChatMessage::assistant("and part two lands here"),
                ChatMessageKind::Continuation,
            ),
            s.now,
        );

        let out = render_to_string(&s);
        assert!(out.contains("part one of the reply"));
        assert!(out.contains("and part two lands here"));
        assert!(
            !out.contains("continuing"),
            "the recovery nudge never renders"
        );
        assert_eq!(
            out.matches('●').count(),
            1,
            "both halves share one assistant bullet:\n{out}"
        );
    }

    #[test]
    fn streaming_continuation_renders_without_fresh_bullet() {
        use crate::domain::{GenPhase, TurnId};
        use crate::models::{ChatMessage, ChatMessageKind};
        let mut s = mock_state();
        s.session.append(ChatMessage::user("audit"), s.now);
        s.session
            .append(ChatMessage::assistant("part one of the reply"), s.now);
        s.session.append(
            kinded(
                ChatMessage::system("output limit — continuing"),
                ChatMessageKind::RecoveryNudge,
            ),
            s.now,
        );
        s.turn = TurnState::Generating {
            id: TurnId(3),
            started: std::time::SystemTime::now(),
            partial_text: "and part two streams in".to_string(),
            partial_reasoning: String::new(),
            tokens: 0,
            phase: GenPhase::Streaming,
            thinking_signature: None,
            pending_tool_calls: Vec::new(),
            continuation: true,
        };

        let out = render_to_string(&s);
        assert!(out.contains("part one of the reply"));
        assert!(out.contains("and part two streams in"));
        assert!(!out.contains("continuing"), "live nudge hidden too");
        assert_eq!(
            out.matches('●').count(),
            1,
            "the streaming half joins the committed bubble:\n{out}"
        );
    }

    #[test]
    fn user_prompt_renders_with_highlight_band() {
        let mut s = mock_state();
        s.session
            .append(crate::models::ChatMessage::user("hello there"), s.now);
        let buf = render_to_buffer(&s);
        let band_bg = crate::render::theme::Theme::dark()
            .colors
            .user_message_background
            .to_color();
        // Row carrying the prompt text.
        let y = (0..buf.area.height)
            .find(|&y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .contains("hello there")
            })
            .expect("user prompt should render");
        // The band fills the row: the great majority of cells carry the band bg
        // (a thin layout margin at the very edges may not).
        let banded = (0..buf.area.width)
            .filter(|&x| buf[(x, y)].bg == band_bg)
            .count();
        assert!(
            banded >= (buf.area.width as usize) * 3 / 4,
            "user prompt band should fill most of the row; only {banded}/{} cells banded",
            buf.area.width
        );
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
        s.session.append(first_msg, s.now);
        let mut second_msg = crate::models::ChatMessage::assistant("second visible answer");
        second_msg.thinking = Some("second private chain of thought".to_string());
        s.session.append(second_msg, s.now);
        let frame = render_to_string(&s);
        // Hidden reasoning is collapsed silently — no placeholder line.
        assert!(!frame.contains("Reasoning hidden"));
        assert!(frame.contains("first visible answer"));
        assert!(frame.contains("second visible answer"));
        assert!(!frame.contains("first private chain of thought"));
        assert!(!frame.contains("second private chain of thought"));
    }

    /// A "thought, then immediately called a tool" turn (hidden reasoning +
    /// empty text + actions) renders the action directly — the turn is not
    /// skipped, and there is no "reasoning hidden" placeholder ahead of it.
    #[test]
    fn hidden_reasoning_then_action_renders_action_without_placeholder() {
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
        s.session.append(msg, s.now);
        let frame = render_to_string(&s);
        assert!(
            !frame.contains("Reasoning hidden"),
            "no reasoning-hidden placeholder"
        );
        assert!(
            frame.contains("Bash"),
            "the action still renders even though reasoning is hidden"
        );
    }

    #[test]
    fn committed_message_appears_in_chat_pane() {
        let mut s = mock_state();
        s.session.append(
            crate::models::ChatMessage::user("unique-user-token-xyz"),
            s.now,
        );
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
}
