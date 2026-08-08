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

pub mod markdown;
pub mod theme;
pub mod widgets;
pub mod wrap;

use ratatui::{
    Frame,
    layout::{Margin, Rect},
    style::Style,
    text::{Line, Span},
};
use rustc_hash::FxHashMap;
use unicode_width::UnicodeWidthChar;

use mermaid_domain::{State, TurnState};
use mermaid_model::models::{ReasoningCapability, ReasoningLevel, nearest_effort};

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
    /// `(state.ui.theme, state.ui.no_color)` the current `theme` was resolved
    /// from. `render()` diffs it each frame and swaps the palette (clearing
    /// `wrapped_line_cache`) only on change, so `/theme` repaints instantly
    /// without per-frame `Theme` construction. `None` (fresh cache) keeps the
    /// `Theme::dark()` default until the first frame resolves it.
    applied_theme: Option<(mermaid_domain::ThemeChoice, bool)>,
    /// Host + user for the status bar's `user@host:cwd` line, read once at
    /// startup so `StatusWidget::render` doesn't hit the environment on every
    /// frame (#55). Process-constant, so caching here is exact.
    pub hostname: String,
    pub username: String,
    /// App version for the status footer. Defaults to the compile-time crate
    /// version; the snapshot suite pins it (like hostname/username) so pinned
    /// frames survive release bumps.
    pub version: String,
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
            version: env!("CARGO_PKG_VERSION").to_string(),
            stitched: None,
            applied_theme: None,
            last_mouse_scroll_accum: 0,
            last_scroll_to_bottom_seq: 0,
        }
    }
}

/// See [`RenderCache::stitched`].
struct StitchedMemo {
    key: u64,
    messages: Vec<mermaid_model::models::ChatMessage>,
}

impl RenderCache {
    pub fn new() -> Self {
        Self::default()
    }
}

/// The entrypoint. Call once per render pass from the main loop.
#[expect(
    clippy::too_many_lines,
    reason = "predates the lint; see .github/baselines/expect_budget.txt"
)]
pub fn render(state: &State, rstate: &mut RenderCache, frame: &mut Frame) {
    // Resolve the palette from reducer state: NO_COLOR beats the theme
    // choice (colors off entirely); otherwise `/theme` picks dark/light.
    let want = (state.ui.theme, state.ui.no_color);
    if rstate.applied_theme != Some(want) {
        rstate.theme = if state.ui.no_color {
            theme::Theme::plain()
        } else {
            match state.ui.theme {
                mermaid_domain::ThemeChoice::Dark => theme::Theme::dark(),
                mermaid_domain::ThemeChoice::Light => theme::Theme::light(),
            }
        };
        // The wrapped-line cache is theme-keyed, but drop stale entries
        // eagerly rather than letting the old palette's lines linger.
        rstate.wrapped_line_cache.clear();
        rstate.applied_theme = Some(want);
    }

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

    // Interrupt modals, decided up front because they reshape the whole
    // bottom of the screen. Approval wins over question when both queue up.
    let approval_item = state.pending_approval.front();
    let question_item = if approval_item.is_none() {
        state.pending_question.front()
    } else {
        None
    };
    // Claude Code parity: while the question modal is up it owns the bottom
    // of the screen — no status spinner, no task band, no input box. Keys
    // route exclusively to the modal anyway (see `handle_question_key`), so
    // the hidden input is inert, not just invisible.
    let question_modal_open = question_item.is_some();

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
    let input_height = if question_modal_open {
        0
    } else {
        (input_lines + 2) as u16
    };

    // Build the status-line rows up front (wrapped to the terminal width) so
    // the layout reserves exactly the height they need — a long task headline
    // plus the trailing `(esc to interrupt …)` fold onto continuation rows
    // instead of bleeding off the right edge.
    let status_lines = if question_modal_open {
        Vec::new()
    } else if state.is_busy() {
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
        let (agent_rows, status_override, bg_available) = agent_panel_data(state);
        // Claude Code parity: while a checklist task is in_progress its
        // active_form IS the spinner headline ("Wiring the broker…"), with
        // the executing tool folded in after a separator.
        let task_headline = state
            .session
            .conversation
            .tasks
            .active()
            .map(|t| t.active_form.clone());
        // Tokens generated so far this run: completed phases carry real
        // provider output counts via `run_tokens` (chars/4 only when a phase
        // reported no usage); the live phase's char-based count rides on top
        // and reconciles to the provider number at its `Done`. While tools
        // run, running subagents' throttled live counts ride on top the same
        // way so the counter keeps climbing instead of freezing for the whole
        // child run (they reconcile when the child's real usage folds in).
        // Marked `~` whenever any estimated component is included.
        let committed = state.runtime.run_tokens;
        let live_child_tokens: usize = state.ui.live_tool_status.values().map(|l| l.tokens).sum();
        let (tokens_display, tokens_estimated) = match &state.turn {
            TurnState::Generating { tokens, .. } => (committed.output_tokens + *tokens, true),
            TurnState::ExecutingTools { .. } => (
                committed.output_tokens + live_child_tokens,
                committed.contains_estimate || live_child_tokens > 0,
            ),
            _ => (0, false),
        };
        build_status_lines(
            GenerationStatus::from_turn(&state.turn),
            elapsed_secs,
            tokens_display,
            tokens_estimated,
            status_override.as_deref(),
            &agent_rows,
            bg_available,
            task_headline.as_deref(),
            &state.ui.queued_messages,
            exit_armed(state),
            &rstate.theme,
            // Match the 1-cell horizontal pad the status zone is rendered with.
            frame.area().width.saturating_sub(2),
        )
    } else if !state.runtime.background_agents.is_empty() {
        // Idle, but detached background agents are still running: keep their
        // rows visible between turns (no spinner head).
        let (agent_rows, _, _) = agent_panel_data(state);
        build_status_lines(
            GenerationStatus::Idle,
            0,
            0,
            false,
            None,
            &agent_rows,
            false,
            None,
            &state.ui.queued_messages,
            exit_armed(state),
            &rstate.theme,
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
        .min(14)
        .min(frame.area().height.saturating_sub(status_reserve));

    // Task checklist band, directly under the status line. Same starvation
    // guard as the status zone: chat floor + input + bottom bar always win.
    // The `⎿` connector only draws when the status zone above actually
    // renders (attached); collapsed + detached shows nothing at all.
    let tasks_store = &state.session.conversation.tasks;
    let tasks_attached = status_line_height > 0;
    let tasks_zone_height = if question_modal_open {
        0
    } else if widgets::tasks_visible(
        tasks_store,
        &state.turn,
        state.ui.tasks_collapsed,
        tasks_attached,
    ) {
        widgets::tasks_height(tasks_store, state.ui.tasks_collapsed).min(
            frame
                .area()
                .height
                .saturating_sub(status_reserve + status_line_height),
        )
    } else {
        0
    };

    // Bottom region: one of three widgets based on UI mode.
    //   - ConversationList picker: 12-line pane.
    //   - Slash palette (input starts with `/`): 3–10 lines based on
    //     filter match count.
    //   - Otherwise: 2-line status bar.
    // Precedence: approval modal > confirm modal > ConversationList picker >
    // slash palette > status bar. Approvals/confirms are interrupts that
    // overlay regardless of input mode. (`approval_item`/`question_item`
    // were decided up front, before the status/input zones were sized.)
    let confirm_open =
        approval_item.is_none() && question_item.is_none() && state.confirm.is_some();
    let conv_list_open = approval_item.is_none()
        && question_item.is_none()
        && !confirm_open
        && matches!(
            state.ui.mode,
            mermaid_domain::UiMode::ConversationList { .. }
        );
    let rewind_open = approval_item.is_none()
        && question_item.is_none()
        && !confirm_open
        && matches!(state.ui.mode, mermaid_domain::UiMode::RewindPicker { .. });
    let plan_config_open = approval_item.is_none()
        && question_item.is_none()
        && !confirm_open
        && matches!(state.ui.mode, mermaid_domain::UiMode::PlanConfig { .. });
    let model_picker_open = approval_item.is_none()
        && question_item.is_none()
        && !confirm_open
        && matches!(state.ui.mode, mermaid_domain::UiMode::ModelPicker { .. });
    let file_picker_open = approval_item.is_none()
        && question_item.is_none()
        && !confirm_open
        && !conv_list_open
        && !rewind_open
        && !plan_config_open
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
        // The modal spans the full frame width; wrapping must be measured at
        // the same width it is drawn at or the reserved zone clips it.
        widgets::question_modal_height(qset, &rstate.theme, frame.area().width)
    } else if confirm_open {
        6
    } else if conv_list_open || rewind_open {
        12
    } else if plan_config_open {
        widgets::PLAN_CONFIG_HEIGHT
    } else if model_picker_open {
        widgets::MODEL_PICKER_HEIGHT
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
        let row_count =
            mermaid_domain::slash_commands::filter_entries(typed, &state.plugin_commands)
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
            Constraint::Length(tasks_zone_height),
            Constraint::Length(input_height),
            Constraint::Length(bottom_height),
        ])
        .split(frame.area());

    // Chat area with 1-cell horizontal padding.
    let chat_area = chunks[0].inner(Margin {
        horizontal: 1,
        vertical: 0,
    });
    // A live toast borrows the chat area's LAST row rather than claiming its
    // own layout slot: the zone above the input already stacks three
    // conditional bands, and a fourth that appears for two seconds would shove
    // the whole transcript. Borrowing a row keeps the input box still.
    let toast = active_toast(state);
    let (chat_area, toast_area) = match toast {
        Some(_) if chat_area.height > 1 => (
            Rect {
                height: chat_area.height - 1,
                ..chat_area
            },
            Some(Rect {
                y: chat_area.y + chat_area.height - 1,
                height: 1,
                ..chat_area
            }),
        ),
        _ => (chat_area, None),
    };
    // Stitch pre-pass: fold auto-continued replies into one bubble and hide
    // spent recovery nudges. Sessions without either kind skip this entirely
    // (borrowed slice, no fingerprint); with them, the memo makes idle frames
    // a hash-check instead of a transcript clone.
    let committed = state.session.messages();
    let base: &[mermaid_model::models::ChatMessage] = if needs_stitch(committed, &state.turn) {
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
    // 500ms blink phase for in-flight action dots, from the injected clock
    // (never the wall clock) so a frame stays a pure function of State.
    let blink_on = (state.now.timestamp_millis().div_euclid(500)) % 2 == 0;
    let chat_widget = ChatWidget {
        messages: live_messages.as_ref(),
        content_key: chat_content_key(state, base, live_messages.as_ref(), blink_on),
        theme: &rstate.theme,
        wrapped_line_cache: &mut rstate.wrapped_line_cache,
        show_reasoning: state.ui.show_reasoning,
        blink_on,
    };
    frame.render_stateful_widget(chat_widget, chat_area, &mut rstate.chat);

    // Toast: right-aligned and dim on the row it borrowed, so it reads as
    // feedback beside the input rather than as a transcript entry.
    if let (Some(text), Some(area)) = (toast, toast_area) {
        frame.render_widget(
            ratatui::widgets::Paragraph::new(Line::from(Span::styled(
                text,
                Style::new().fg(rstate.theme.colors.info.to_color()),
            )))
            .alignment(ratatui::layout::Alignment::Right),
            area,
        );
    }

    // Status line for every active turn (built above, already fit to width).
    // Indented 1 cell to align with the chat column's 1-cell pad.
    if !status_lines.is_empty() {
        let status_area = chunks[1].inner(Margin {
            horizontal: 1,
            vertical: 0,
        });
        frame.render_widget(ratatui::widgets::Paragraph::new(status_lines), status_area);
    }

    // Task checklist band (chunks[2]), hanging under the spinner line.
    if tasks_zone_height > 0 {
        let tasks_area = chunks[2].inner(Margin {
            horizontal: 1,
            vertical: 0,
        });
        let lines = widgets::build_task_lines(
            tasks_store,
            state.ui.tasks_collapsed,
            tasks_attached,
            tasks_area.width,
            &rstate.theme,
        );
        frame.render_widget(ratatui::widgets::Paragraph::new(lines), tasks_area);
    }

    // Input box (chunks[3]; the attachment zone is gone, the task band
    // precedes). Collapsed entirely — including the terminal cursor — while
    // the question modal owns the bottom of the screen.
    if !question_modal_open {
        let input_widget = InputWidget {
            input: state.ui.input_buffer.as_str(),
            showing_command_hints: state.ui.input_buffer.starts_with('/'),
            theme: &rstate.theme,
            reasoning_active: state.session.reasoning != ReasoningLevel::None,
            exit_armed: exit_armed(state),
            rewind_armed: rewind_armed(state),
        };
        let mut input_widget_state = InputState {
            cursor_position: state.ui.input_cursor.min(state.ui.input_buffer.len()),
        };
        frame.render_stateful_widget(input_widget, chunks[3], &mut input_widget_state);

        // Cursor tracks the input caret.
        let input_area = chunks[3];
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
        frame.render_widget(widget, chunks[4]);
    } else if let Some(qset) = state.pending_question.front() {
        use widgets::QuestionModalWidget;
        let widget = QuestionModalWidget {
            theme: &rstate.theme,
            set: qset,
            width: chunks[4].width,
        };
        frame.render_widget(widget, chunks[4]);
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
        frame.render_widget(widget, chunks[4]);
    } else if let mermaid_domain::UiMode::ModelPicker {
        candidates,
        query,
        cursor,
        loading,
    } = &state.ui.mode
    {
        use widgets::ModelPickerWidget;
        let matches = mermaid_domain::reducer::filter_model_choices(candidates, query);
        let widget = ModelPickerWidget {
            theme: &rstate.theme,
            matches: &matches,
            query,
            cursor: *cursor,
            loading: *loading,
            current: &state.session.model_id,
        };
        frame.render_widget(widget, chunks[4]);
    } else if let mermaid_domain::UiMode::ConversationList { candidates, cursor } = &state.ui.mode {
        use widgets::ConversationListWidget;
        let widget = ConversationListWidget {
            theme: &rstate.theme,
            candidates,
            cursor: *cursor,
        };
        frame.render_widget(widget, chunks[4]);
    } else if let mermaid_domain::UiMode::RewindPicker { candidates, cursor } = &state.ui.mode {
        use widgets::RewindPickerWidget;
        let widget = RewindPickerWidget {
            theme: &rstate.theme,
            candidates,
            cursor: *cursor,
        };
        frame.render_widget(widget, chunks[4]);
    } else if let mermaid_domain::UiMode::PlanConfig { cursor } = &state.ui.mode {
        use widgets::PlanConfigWidget;
        let widget = PlanConfigWidget {
            theme: &rstate.theme,
            plan: &state.settings.plan,
            session_model: &state.session.model_id,
            cursor: *cursor,
        };
        frame.render_widget(widget, chunks[4]);
    } else if file_picker_open {
        use widgets::FilePickerWidget;
        let widget = FilePickerWidget {
            theme: &rstate.theme,
            matches: &state.ui.file_picker_matches,
            selected_index: state.ui.file_picker_cursor.unwrap_or(0),
            loading: state.ui.project_files_loading && state.ui.project_files.is_none(),
        };
        frame.render_widget(widget, chunks[4]);
    } else if palette_open {
        let typed = state
            .ui
            .input_buffer
            .trim_start_matches('/')
            .split_whitespace()
            .next()
            .unwrap_or("");
        let entries = mermaid_domain::slash_commands::filter_entries(typed, &state.plugin_commands);
        let palette_widget = SlashPaletteWidget {
            theme: &rstate.theme,
            entries,
            selected_index: state.ui.palette_cursor.unwrap_or(0),
        };
        frame.render_widget(palette_widget, chunks[4]);
    } else {
        let cwd = state.cwd.display().to_string();
        let status_widget = StatusWidget {
            theme: &rstate.theme,
            working_dir: &cwd,
            hostname: &rstate.hostname,
            username: &rstate.username,
            version: &rstate.version,
            context_usage: state.session.context_usage.as_ref(),
            model_name: &state.session.model_id,
            reasoning_level: effective,
            requested_level,
            // Planning IS the mode — `plan` renders through the same
            // `safety: <mode>` segment as every other level.
            safety_mode: state.session.safety_mode,
        };
        frame.render_widget(status_widget, chunks[4]);
    }
}

/// Can a `Continuation` message be folded into this predecessor? Guards the
/// stitch against non-bubble assistants: a compaction checkpoint's assistant
/// half (`ContextCheckpoint`, rendered as an event block), the empty
/// error-carrier message, or an assistant that ended in tool calls.
/// `pub(crate)` so the chat widget applies the same rule when deciding to
/// draw a streaming continuation without a fresh bubble prefix.
pub(crate) fn mergeable_into(prev: &mermaid_model::models::ChatMessage) -> bool {
    prev.role == mermaid_model::models::MessageRole::Assistant
        && matches!(
            prev.kind,
            mermaid_model::models::ChatMessageKind::Normal
                | mermaid_model::models::ChatMessageKind::Continuation
        )
        && prev.tool_calls.is_none()
}

/// Identify the transcript the chat widget is about to paint, in O(1).
///
/// The widget's frame memo needs a key that changes whenever the rendered
/// content changes. Hashing every message did that honestly but cost
/// O(transcript) on every frame — 34% of an idle frame at a 2000-message
/// scrollback, and the last thing scaling with history size.
///
/// Three inputs, all constant-time:
/// - `ConversationHistory::revision`, bumped by the accessor that hands out
///   `&mut` to the messages, so no committed change can escape it.
/// - The messages `build_live_messages` derived on top of the committed slice
///   (a streaming partial, or a live action row) — at most one, and not part
///   of history, so it must be hashed directly.
/// - The blink phase, folded in ONLY while a turn is active. Running action
///   dots exist during a turn, and an active turn already invalidates the memo
///   continuously; folding it in unconditionally would invalidate twice a
///   second on every idle frame, which is exactly what this exists to avoid.
///   The cosmetic cost is that a `Running` action left behind by a cancelled
///   run stops blinking once the session goes idle.
fn chat_content_key(
    state: &State,
    base: &[mermaid_model::models::ChatMessage],
    live: &[mermaid_model::models::ChatMessage],
    blink_on: bool,
) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = rustc_hash::FxHasher::default();
    state.session.conversation.revision().hash(&mut h);
    // The stitch is a pure function of committed history, but its output
    // length is not, so fold it in rather than assuming.
    base.len().hash(&mut h);
    for msg in live.iter().skip(base.len()) {
        msg.content.hash(&mut h);
        msg.thinking.hash(&mut h);
        std::mem::discriminant(&msg.kind).hash(&mut h);
        msg.actions.len().hash(&mut h);
        for action in &msg.actions {
            action.action_type.hash(&mut h);
            action.target.hash(&mut h);
            std::mem::discriminant(&action.result).hash(&mut h);
        }
    }
    if !matches!(state.turn, TurnState::Idle) {
        blink_on.hash(&mut h);
    }
    h.finish()
}

/// Would the stitch pre-pass change anything the user can see? If not,
/// rendering borrows the committed slice with zero copies.
///
/// Only CONTINUATIONS need it. `RecoveryNudge` and `ContextMarker` are hidden
/// either way — `ChatWidget` skips exactly those two kinds itself — so
/// removing them upstream matters only when something downstream inspects a
/// message's neighbours, and only continuation merging does:
///
/// - `stitch_committed` merges a committed `Continuation` into `out.last_mut()`.
/// - `build_live_messages` merges a LIVE continuation when
///   `committed.last().is_some_and(mergeable_into)` — and during auto-continue
///   the last committed message is the "hit the output limit" nudge, which
///   must be stripped or the merge fails and the partial re-renders as a fresh
///   bubble with duplicated overlap text. Hence the turn state, not just
///   history: the live continuation streams BEFORE any `Continuation` is
///   committed.
///
/// Including markers here made this permanently true for any session that ever
/// changed mode — `ContextMarker` is never swept — which cost a
/// transcript-sized hash on every frame forever, at ~60 frames per second.
fn needs_stitch(committed: &[mermaid_model::models::ChatMessage], turn: &TurnState) -> bool {
    let live_continuation = matches!(
        turn,
        TurnState::Generating { continuation, .. } if *continuation
    );
    live_continuation
        || committed
            .iter()
            .any(|m| m.kind == mermaid_model::models::ChatMessageKind::Continuation)
}

/// Fingerprint of every committed-message field the stitched transcript
/// depends on. Cheap relative to re-stitching (hashing, no cloning); mirrors
/// the chat widget's frame fingerprint so in-place mutations that don't
/// change message count (e.g. an action attached to the last message during a
/// tool run) still invalidate the memo.
fn stitch_fingerprint(committed: &[mermaid_model::models::ChatMessage]) -> u64 {
    use std::hash::{Hash, Hasher};
    use std::mem::discriminant;

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
        discriminant(&msg.role).hash(&mut h);
        discriminant(&msg.kind).hash(&mut h);
        msg.tool_calls.as_ref().map(|t| t.len()).hash(&mut h);
        // Actions used to be folded in with `{:?}`, which Debug-formatted the
        // whole `ToolRunMetadata` — INCLUDING `display_diff`, a full diff
        // string — for every action on every message, every frame. Since a
        // persistent `ContextMarker` keeps `needs_stitch` true for the rest of
        // the session, that ran continuously. Hash the fields that actually
        // change instead: `display_diff` is captured once at tool-execution
        // time and never mutates afterward, so its length is a sound stand-in.
        msg.actions.len().hash(&mut h);
        for action in &msg.actions {
            action.action_type.hash(&mut h);
            action.target.hash(&mut h);
            discriminant(&action.result).hash(&mut h);
            discriminant(&action.details).hash(&mut h);
            action.duration_seconds.map(f64::to_bits).hash(&mut h);
            if let Some(meta) = &action.metadata {
                meta.lines_added.hash(&mut h);
                meta.lines_removed.hash(&mut h);
                meta.diff_truncated.hash(&mut h);
                meta.display_diff.as_ref().map(String::len).hash(&mut h);
            }
        }
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
fn stitch_committed(
    committed: &[mermaid_model::models::ChatMessage],
) -> Vec<mermaid_model::models::ChatMessage> {
    let mut out: Vec<mermaid_model::models::ChatMessage> = Vec::with_capacity(committed.len());
    for msg in committed {
        if matches!(
            msg.kind,
            mermaid_model::models::ChatMessageKind::RecoveryNudge
                | mermaid_model::models::ChatMessageKind::ContextMarker
        ) {
            continue;
        }
        if msg.kind == mermaid_model::models::ChatMessageKind::Continuation
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
fn merge_continuation(
    prev: &mut mermaid_model::models::ChatMessage,
    cont: &mermaid_model::models::ChatMessage,
) {
    let skip = mermaid_model::utils::continuation_overlap(&prev.content, &cont.content);
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

/// Merge the committed message log with the live turn's in-flight view:
/// partial streamed content from `TurnState::Generating`, or the executing
/// batch's action rows from `TurnState::ExecutingTools`. The chat widget
/// renders this as a single stream.
///
/// While tools run, each call gets its transcript action row immediately —
/// completed calls with their real outcome, still-running ones as a
/// `Running` placeholder whose header dot blinks (Claude Code parity: the
/// transcript, not the status spinner, names the tool). Two kinds of pending
/// call are skipped: `agent` (the live agent panel under the spinner carries
/// them) and `ask_user_question` (the modal IS its in-flight representation;
/// the question → answer block lands once answered).
///
/// `committed` is the (possibly stitched) display transcript. When the live
/// turn is an auto-continue, the pseudo-message is stamped `Continuation` so
/// the widget draws it as a prefix-less extension of the previous bubble, and
/// its leading resume-echo is trimmed against that bubble's tail — the
/// in-flight reply looks like one message while it streams, not just after
/// it commits.
fn build_live_messages<'a>(
    committed: &'a [mermaid_model::models::ChatMessage],
    turn: &TurnState,
    now: chrono::DateTime<chrono::Local>,
) -> std::borrow::Cow<'a, [mermaid_model::models::ChatMessage]> {
    if let TurnState::ExecutingTools {
        calls, outcomes, ..
    } = turn
    {
        let actions: Vec<mermaid_domain::ActionDisplay> = calls
            .iter()
            .zip(outcomes)
            .filter_map(|(call, outcome)| match outcome {
                Some(outcome) => Some(mermaid_domain::transition::action_display_for(
                    call, outcome,
                )),
                None => {
                    let name = call.source.function.name.as_str();
                    if name == "agent" || name == "ask_user_question" {
                        return None;
                    }
                    let (action_type, target) = mermaid_domain::display_info_for(call);
                    Some(mermaid_domain::ActionDisplay {
                        action_type,
                        target,
                        result: mermaid_domain::ActionResult::Running,
                        details: mermaid_domain::ActionDetails::Simple,
                        duration_seconds: None,
                        metadata: None,
                    })
                },
            })
            .collect();
        if actions.is_empty() {
            return std::borrow::Cow::Borrowed(committed);
        }
        let mut msg = mermaid_model::models::ChatMessage::assistant("");
        msg.timestamp = now;
        msg.actions = actions;
        let mut out = committed.to_vec();
        out.push(msg);
        return std::borrow::Cow::Owned(out);
    }
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
            let skip = mermaid_model::utils::continuation_overlap(prev, partial_text);
            partial_text[skip..].to_string()
        } else {
            partial_text.clone()
        };
        let msg = mermaid_model::models::ChatMessage {
            role: mermaid_model::models::MessageRole::Assistant,
            content,
            // `state.now` (stamped each tick) keeps render a pure function of
            // State — never read the wall clock here.
            timestamp: now,
            kind: if stitching {
                mermaid_model::models::ChatMessageKind::Continuation
            } else {
                mermaid_model::models::ChatMessageKind::Normal
            },
            metadata: None,
            actions: Vec::new(),
            thinking,
            images: None,
            image_numbers: None,
            tool_calls: None,
            tool_call_id: None,
            tool_name: None,
            provider_continuation: None,
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

/// The toast to draw, or `None` once it has expired. Same lazy-expiry pattern
/// as `exit_armed`: nothing clears `ui.toast`, the 60 Hz tick just stops
/// drawing it once `state.now` passes the deadline.
fn active_toast(state: &State) -> Option<String> {
    state
        .ui
        .toast
        .as_ref()
        .filter(|(_, until)| state.now <= *until)
        .map(|(text, _)| text.clone())
}

/// True while a first idle Esc's rewind window is open (same lazy-expiry
/// pattern as `exit_armed`; the reducer owns the 1s window constant).
fn rewind_armed(state: &State) -> bool {
    state
        .ui
        .esc_armed_at
        .is_some_and(|armed| (state.now - armed) <= chrono::Duration::milliseconds(1000))
}

/// Data for the live agent panel + the status-line adjustments it implies:
/// one `AgentPanelRow` per in-flight `agent` call (plus every detached
/// background agent), a status override ("Running N agents") when agents are
/// the only pending work, and whether anything running can honor Ctrl+B.
fn agent_panel_data(state: &State) -> (Vec<widgets::AgentPanelRow>, Option<String>, bool) {
    let now_sys = std::time::SystemTime::from(state.now);
    let elapsed_since =
        |t: std::time::SystemTime| now_sys.duration_since(t).map(|d| d.as_secs()).unwrap_or(0);

    let mut rows = Vec::new();
    let mut running_agents = 0usize;
    let mut pending_total = 0usize;
    let mut bg_available = false;
    if let TurnState::ExecutingTools {
        calls,
        outcomes,
        started,
        ..
    } = &state.turn
    {
        let elapsed = elapsed_since(*started);
        for (call, _) in calls.iter().zip(outcomes).filter(|(_, o)| o.is_none()) {
            pending_total += 1;
            let name = call.source.function.name.as_str();
            if name == "execute_command" || name == "agent" {
                bg_available = true;
            }
            if name != "agent" {
                continue;
            }
            running_agents += 1;
            let (_, description) = mermaid_domain::display_info_for(call);
            let live = state.ui.live_tool_status.get(&call.call_id);
            rows.push(widgets::AgentPanelRow {
                description,
                activity: live.map(|l| l.activity.clone()).unwrap_or_default(),
                tokens: live.map_or(0, |l| l.tokens),
                elapsed_secs: elapsed,
                backgrounded: false,
            });
        }
    }
    for agent in &state.runtime.background_agents {
        rows.push(widgets::AgentPanelRow {
            description: agent.description.clone(),
            activity: agent.activity.clone(),
            tokens: agent.tokens,
            elapsed_secs: elapsed_since(agent.started),
            backgrounded: true,
        });
    }
    let status_override = (running_agents > 0 && running_agents == pending_total).then(|| {
        if running_agents == 1 {
            "Running 1 agent".to_string()
        } else {
            format!("Running {running_agents} agents")
        }
    });
    (rows, status_override, bg_available)
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

/// Full-frame snapshots of `render()`. Runs on every platform (#296): the
/// suite pins its own clock, host/user, version and cwd, so nothing platform-
/// dependent reaches the frame.
#[cfg(test)]
mod snapshots;

/// Idle-frame measurement rig (`#[ignore]`d). See `bench.rs` for how to run it.
#[cfg(test)]
mod bench;

#[cfg(test)]
mod tests {
    use super::*;
    use mermaid_domain::Config;
    use mermaid_domain::{State, TurnState};
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
    fn theme_choice_changes_colors_never_glyphs() {
        // Guards the "theme changes can't break snapshots" claim: light,
        // dark, and NO_COLOR-plain frames must be glyph-identical — a theme
        // is a palette, not a layout.
        let mut state = mock_state();
        state
            .session
            .append(mermaid_model::models::ChatMessage::user("hello"), state.now);
        let dark = render_to_string(&state);
        state.ui.theme = mermaid_domain::ThemeChoice::Light;
        let light = render_to_string(&state);
        assert_eq!(dark, light, "light theme changed glyphs");
        state.ui.no_color = true;
        let plain = render_to_string(&state);
        assert_eq!(dark, plain, "NO_COLOR changed glyphs");
    }

    #[test]
    fn theme_memo_swaps_palette_on_state_change() {
        let mut state = mock_state();
        let mut rstate = RenderCache::new();
        render_frame(&state, &mut rstate, 80, 24);
        assert_eq!(rstate.theme.name, "Dark");
        state.ui.theme = mermaid_domain::ThemeChoice::Light;
        render_frame(&state, &mut rstate, 80, 24);
        assert_eq!(rstate.theme.name, "Light");
        // NO_COLOR beats the theme choice.
        state.ui.no_color = true;
        render_frame(&state, &mut rstate, 80, 24);
        assert_eq!(rstate.theme.name, "Plain");
    }

    #[test]
    fn agent_calls_get_panel_rows_and_a_calm_status_override() {
        use mermaid_domain::{LiveToolStatus, PendingToolCall, ToolCallId, TurnId};

        let mut state = mock_state();
        let call_id = ToolCallId(7);
        state.turn = TurnState::ExecutingTools {
            id: TurnId(1),
            started: std::time::SystemTime::now(),
            calls: vec![PendingToolCall {
                call_id,
                source: mermaid_model::models::tool_call::ToolCall {
                    id: None,
                    function: mermaid_model::models::tool_call::FunctionCall {
                        name: "agent".to_string(),
                        arguments: serde_json::json!({"description": "explore crates"}),
                    },
                },
            }],
            outcomes: vec![None],
        };
        state.ui.live_tool_status.insert(
            call_id,
            LiveToolStatus {
                activity: "read_file…".to_string(),
                tokens: 12_300,
            },
        );

        // Agent calls never get a live transcript action row — they get panel
        // rows (and the "Running N agents" override) instead.
        let live = build_live_messages(&[], &state.turn, chrono::Local::now());
        assert!(
            live.is_empty(),
            "a pending agent call must not synthesize a transcript row"
        );
        let (rows, override_text, bg_available) = agent_panel_data(&state);
        assert_eq!(override_text.as_deref(), Some("Running 1 agent"));
        assert!(bg_available, "agents are detachable via ctrl+b");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].description, "explore crates");
        assert_eq!(rows[0].activity, "read_file…");
        assert_eq!(rows[0].tokens, 12_300);
        assert!(!rows[0].backgrounded);
    }

    #[test]
    fn mixed_turn_names_first_non_agent_tool_with_stable_activity() {
        use mermaid_domain::{LiveToolStatus, PendingToolCall, ToolCallId, TurnId};

        let mut state = mock_state();
        let exec_id = ToolCallId(8);
        let agent_id = ToolCallId(9);
        let call = |id, name: &str, args| PendingToolCall {
            call_id: id,
            source: mermaid_model::models::tool_call::ToolCall {
                id: None,
                function: mermaid_model::models::tool_call::FunctionCall {
                    name: name.to_string(),
                    arguments: args,
                },
            },
        };
        state.turn = TurnState::ExecutingTools {
            id: TurnId(1),
            started: std::time::SystemTime::now(),
            calls: vec![
                call(
                    exec_id,
                    "execute_command",
                    serde_json::json!({"command": "cargo test"}),
                ),
                call(
                    agent_id,
                    "agent",
                    serde_json::json!({"description": "audit docs"}),
                ),
            ],
            outcomes: vec![None, None],
        };
        state.ui.live_tool_status.insert(
            exec_id,
            LiveToolStatus {
                activity: String::new(),
                tokens: 0,
            },
        );

        // The shell command gets a live Running transcript row; the agent gets
        // only its panel row. No override since agents aren't the only work.
        let live = build_live_messages(&[], &state.turn, chrono::Local::now());
        assert_eq!(live.len(), 1, "one synthetic message carries the rows");
        let actions = &live[0].actions;
        assert_eq!(actions.len(), 1, "the agent call gets no transcript row");
        assert_eq!(actions[0].action_type, "Bash");
        assert_eq!(actions[0].target, "cargo test");
        assert!(matches!(
            actions[0].result,
            mermaid_domain::ActionResult::Running
        ));
        let (rows, override_text, _) = agent_panel_data(&state);
        assert_eq!(override_text, None);
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn build_live_messages_borrows_idle_and_stamps_partial_with_injected_now() {
        use mermaid_domain::{GenPhase, TurnId};
        use mermaid_model::models::ChatMessage;
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
            provider_continuation: None,
            pending_tool_calls: Vec::new(),
            continuation: false,
        };
        let live = build_live_messages(&committed, &turn, now);
        assert!(matches!(live, Cow::Owned(_)));
        assert_eq!(live.len(), 2);
        assert_eq!(live[1].timestamp, now);
    }

    fn kinded(
        mut msg: mermaid_model::models::ChatMessage,
        kind: mermaid_model::models::ChatMessageKind,
    ) -> mermaid_model::models::ChatMessage {
        msg.kind = kind;
        msg
    }

    #[test]
    fn stitch_committed_merges_chain_and_hides_nudges() {
        use mermaid_model::models::{ChatMessage, ChatMessageKind};
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

        assert!(needs_stitch(&committed, &TurnState::Idle));
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

    /// Context markers are model-facing timeline records — the status band is
    /// the human announcement of a mode change, so the transcript hides them.
    #[test]
    fn context_markers_are_hidden_from_the_transcript() {
        use mermaid_model::models::{ChatMessage, ChatMessageKind};
        let committed = vec![
            ChatMessage::user("plan this"),
            kinded(
                ChatMessage::system("Plan mode is now ON. Author the plan at x.md."),
                ChatMessageKind::ContextMarker,
            ),
            ChatMessage::assistant("Grounding first."),
        ];
        // Markers are hidden by `ChatWidget` itself, so they do NOT force the
        // copying stitch path — that is the whole point, since a marker is
        // never swept and would otherwise cost a transcript hash on every
        // frame for the rest of the session.
        assert!(
            !needs_stitch(&committed, &TurnState::Idle),
            "a marker alone must not defeat the zero-copy path",
        );
        // The stitch still drops them when it runs for a real continuation.
        let stitched = stitch_committed(&committed);
        assert_eq!(stitched.len(), 2, "user + assistant only");
        assert!(
            !stitched
                .iter()
                .any(|m| m.content.contains("Plan mode is now ON")),
            "markers never render"
        );
    }

    #[test]
    fn stitch_refuses_non_bubble_predecessor() {
        use mermaid_model::models::{ChatMessage, ChatMessageKind};
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
        use mermaid_model::models::ChatMessage;
        // The fast path: a session that never auto-continued skips the
        // pre-pass entirely (borrowed slice, no fingerprint, no clone).
        let committed = vec![
            ChatMessage::user("hi"),
            ChatMessage::assistant("hello"),
            ChatMessage::system("note"),
        ];
        assert!(!needs_stitch(&committed, &TurnState::Idle));
    }

    /// A live auto-continue streams BEFORE any `Continuation` is committed,
    /// and the message just before it is the "hit the output limit" nudge.
    /// `build_live_messages` merges the partial only when
    /// `committed.last()` is a mergeable assistant bubble — so the nudge has
    /// to be stitched out even though nothing in HISTORY is a continuation.
    /// Miss this and the partial renders as a fresh bubble with the overlap
    /// text duplicated.
    #[test]
    fn a_live_continuation_still_forces_the_stitch() {
        use mermaid_model::models::{ChatMessage, ChatMessageKind};
        let committed = vec![
            ChatMessage::user("write it"),
            ChatMessage::assistant("first half"),
            kinded(
                ChatMessage::system("output limit — continuing"),
                ChatMessageKind::RecoveryNudge,
            ),
        ];
        let streaming = TurnState::Generating {
            id: mermaid_domain::TurnId(1),
            started: std::time::SystemTime::UNIX_EPOCH,
            partial_text: "first half and the rest".to_string(),
            partial_reasoning: String::new(),
            tokens: 0,
            phase: mermaid_domain::GenPhase::Streaming,
            provider_continuation: None,
            pending_tool_calls: Vec::new(),
            continuation: true,
        };
        assert!(
            needs_stitch(&committed, &streaming),
            "a live continuation needs the nudge stripped to find its bubble",
        );
        // Without the nudge in the way, the partial merges into the bubble.
        let stitched = stitch_committed(&committed);
        assert!(
            stitched.last().is_some_and(mergeable_into),
            "the stitched tail is the assistant bubble the partial merges into",
        );
    }

    #[test]
    fn build_live_messages_stamps_streaming_continuation_and_trims_echo() {
        use mermaid_domain::{GenPhase, TurnId};
        use mermaid_model::models::{ChatMessage, ChatMessageKind};

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
            provider_continuation: None,
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
        use mermaid_model::models::{ChatMessage, ChatMessageKind};
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
        use mermaid_domain::{GenPhase, TurnId};
        use mermaid_model::models::{ChatMessage, ChatMessageKind};
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
            provider_continuation: None,
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
        s.session.append(
            mermaid_model::models::ChatMessage::user("hello there"),
            s.now,
        );
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
        s.turn = mermaid_domain::transition::start_generating(
            mermaid_domain::TurnId(1),
            std::time::SystemTime::now(),
        );
        let frame = render_to_string(&s);
        assert!(
            frame.contains("Sending") || frame.contains("Thinking") || frame.contains("Streaming"),
            "expected generation status in frame"
        );
    }

    #[test]
    fn in_flight_tool_renders_as_transcript_row_with_bare_status_line() {
        use mermaid_domain::PendingToolCall;
        use mermaid_model::models::tool_call::{FunctionCall, ToolCall as ModelToolCall};
        let mut s = mock_state();
        let call = PendingToolCall {
            call_id: mermaid_domain::ToolCallId(1),
            source: ModelToolCall {
                id: Some("c1".to_string()),
                function: FunctionCall {
                    name: "execute_command".to_string(),
                    arguments: serde_json::json!({"command": "npm run dev"}),
                },
            },
        };
        s.turn = TurnState::ExecutingTools {
            id: mermaid_domain::TurnId(1),
            started: std::time::SystemTime::now(),
            calls: vec![call],
            outcomes: vec![None],
        };
        let frame = render_to_string(&s);
        // The spinner headline is the bare phase word — the command must NOT
        // ride on it (the bug class this regression test pins down)…
        assert!(frame.contains("Running tools..."), "got: {frame}");
        assert!(
            !frame.contains("Running tools:"),
            "status line must not carry tool detail; got: {frame}"
        );
        // …because the transcript's live action row names it instead.
        assert!(
            frame.contains("npm run dev"),
            "transcript must show the in-flight call's action row; got: {frame}"
        );
    }

    #[test]
    fn pending_question_and_agent_calls_get_no_transcript_row() {
        use mermaid_domain::PendingToolCall;
        use mermaid_model::models::tool_call::{FunctionCall, ToolCall as ModelToolCall};
        let mut s = mock_state();
        let mk = |id: u64, name: &str, args: serde_json::Value| PendingToolCall {
            call_id: mermaid_domain::ToolCallId(id),
            source: ModelToolCall {
                id: Some(format!("c{id}")),
                function: FunctionCall {
                    name: name.to_string(),
                    arguments: args,
                },
            },
        };
        s.turn = TurnState::ExecutingTools {
            id: mermaid_domain::TurnId(1),
            started: std::time::SystemTime::now(),
            calls: vec![
                mk(1, "ask_user_question", serde_json::json!({"questions": []})),
                mk(
                    2,
                    "agent",
                    serde_json::json!({"description": "scan the repo"}),
                ),
            ],
            outcomes: vec![None, None],
        };
        let frame = render_to_string(&s);
        // The question's representation is the modal; the agent's is its
        // panel row under the spinner. Neither gets a transcript action row.
        assert!(
            !frame.contains("ask_user_question"),
            "pending question must not surface as a transcript row or status text; got: {frame}"
        );
    }

    #[test]
    fn status_line_appears_during_tool_execution_and_shows_queue() {
        let mut s = mock_state();
        s.turn = TurnState::ExecutingTools {
            id: mermaid_domain::TurnId(1),
            started: std::time::SystemTime::now(),
            calls: Vec::new(),
            outcomes: Vec::new(),
        };
        s.ui.queued_messages
            .push_back(mermaid_domain::QueuedMessage {
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
        let mut first_msg = mermaid_model::models::ChatMessage::assistant("first visible answer");
        first_msg.thinking = Some("first private chain of thought".to_string());
        s.session.append(first_msg, s.now);
        let mut second_msg = mermaid_model::models::ChatMessage::assistant("second visible answer");
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
        let mut msg = mermaid_model::models::ChatMessage::assistant("");
        msg.thinking = Some("private chain of thought".to_string());
        msg.actions.push(mermaid_domain::ActionDisplay {
            action_type: "Bash".to_string(),
            target: "dir".to_string(),
            result: mermaid_domain::ActionResult::Success {
                output: "ok".to_string(),
                images: None,
            },
            details: mermaid_domain::ActionDetails::Simple,
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
            mermaid_model::models::ChatMessage::user("unique-user-token-xyz"),
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
