use crate::cmd::Cmd;
use crate::msg::{ClipboardRead, KeyCode, KeyMods, Msg, Paste};
use crate::picker::{PickerStep, picker_step};
use crate::query::Query;
use crate::reducer::*;
use crate::reports::*;
use crate::state::{Focus, State, TurnState, UiMode};
use crate::transition::start_generating;
use mermaid_model::models::ChatMessage;

// ─── helpers ────────────────────────────────────────────────────────

/// Outcome of one keypress against a question modal: keep showing it, or
/// resolve the whole set one way or the other.
pub enum QuestionKeyAction {
    Stay,
    Submit,
    Dismiss,
    Reformulate,
}

/// Advance past the current question: resolve immediately for the atomic
/// single-select-single-question case, step to the next question, or land on
/// the review screen.
pub fn advance_question(
    set: &mut mermaid_model::question::PendingQuestionSet,
) -> QuestionKeyAction {
    let nq = set.questions.len();
    if set.skips_review() {
        return QuestionKeyAction::Submit;
    }
    if set.active + 1 < nq {
        set.active += 1;
    } else {
        set.active = nq; // review screen
    }
    QuestionKeyAction::Stay
}

/// Act on an option row: toggle it (multi-select) or choose it and advance
/// (single-select, which also drops any typed "Other" text).
pub fn act_on_option(
    set: &mut mermaid_model::question::PendingQuestionSet,
    q_idx: usize,
    opt_idx: usize,
) -> QuestionKeyAction {
    let multi = set.questions[q_idx].is_multi();
    let sel = &mut set.selections[q_idx];
    if multi {
        if let Some(pos) = sel.chosen.iter().position(|&i| i == opt_idx) {
            sel.chosen.remove(pos);
        } else {
            sel.chosen.push(opt_idx);
        }
        QuestionKeyAction::Stay
    } else {
        sel.chosen = vec![opt_idx];
        sel.other_text.clear();
        advance_question(set)
    }
}

/// Act on the row under the cursor: an option, the "Other" free-text row, or
/// the multi-select Submit row.
pub fn act_on_row(
    set: &mut mermaid_model::question::PendingQuestionSet,
    q_idx: usize,
    row: usize,
) -> QuestionKeyAction {
    let n = set.questions[q_idx].options.len();
    let multi = set.questions[q_idx].is_multi();
    if row < n {
        return act_on_option(set, q_idx, row);
    }
    if row == set.other_row(q_idx) {
        // Multi-select captures the typed text directly, so Enter here is a
        // no-op; single-select commits the typed answer (if any) and advances.
        if multi || set.selections[q_idx].other_text.trim().is_empty() {
            return QuestionKeyAction::Stay;
        }
        set.selections[q_idx].chosen.clear();
        return advance_question(set);
    }
    if Some(row) == set.submit_row(q_idx) {
        return advance_question(set);
    }
    QuestionKeyAction::Stay
}

/// Apply one keypress to the front question set, returning whether it resolves.
#[expect(
    clippy::too_many_lines,
    reason = "predates the lint; see .github/baselines/expect_budget.txt"
)]
pub fn apply_question_key(
    set: &mut mermaid_model::question::PendingQuestionSet,
    code: KeyCode,
    mods: KeyMods,
) -> QuestionKeyAction {
    // Note-editing sub-mode: keystrokes edit the active question's note until
    // Enter/Esc exits (Esc here leaves the note intact — it does not dismiss).
    if set.editing_note {
        match code {
            KeyCode::Enter | KeyCode::Escape => set.editing_note = false,
            KeyCode::Char(c) if !mods.ctrl && !mods.alt => {
                if let Some(sel) = set.selections.get_mut(set.active) {
                    sel.note.push(c);
                }
            },
            KeyCode::Backspace => {
                if let Some(sel) = set.selections.get_mut(set.active) {
                    sel.note.pop();
                }
            },
            _ => {},
        }
        return QuestionKeyAction::Stay;
    }

    // Esc dismisses the whole set.
    if code == KeyCode::Escape && mods.is_empty() {
        return QuestionKeyAction::Dismiss;
    }

    let nq = set.questions.len();

    // `n` opens note editing for the active question — but not on the review
    // screen, and not when the cursor sits in the Other text field (where `n`
    // is a literal character).
    if code == KeyCode::Char('n')
        && mods.is_empty()
        && set.active < nq
        && set.questions[set.active].is_choice()
        && set.selections[set.active].cursor != set.other_row(set.active)
    {
        set.editing_note = true;
        return QuestionKeyAction::Stay;
    }

    // `c` = "Chat about this": bounce the whole set back to the model to
    // reformulate. Available on choice/rank tabs (not the Other field) and on
    // the review screen; on input tabs `c` is a literal character.
    if code == KeyCode::Char('c')
        && mods.is_empty()
        && (set.active >= nq
            || (set.questions[set.active].is_choice()
                && set.selections[set.active].cursor != set.other_row(set.active)))
    {
        return QuestionKeyAction::Reformulate;
    }

    // `r` = toggle "remember my answers across sessions" (available where `c`
    // is). The tool persists answers keyed by each question's `memory_key`.
    if code == KeyCode::Char('r')
        && mods.is_empty()
        && (set.active >= nq
            || (set.questions[set.active].is_choice()
                && set.selections[set.active].cursor != set.other_row(set.active)))
    {
        set.remember = !set.remember;
        return QuestionKeyAction::Stay;
    }

    // Tab-strip navigation between questions / the review screen. Tab + Right
    // move forward; BackTab + Left move back (no in-field cursor in Stage 1).
    let go_next = code == KeyCode::Tab || (mods.is_empty() && code == KeyCode::Right);
    let go_prev = code == KeyCode::BackTab || (mods.is_empty() && code == KeyCode::Left);
    if go_next {
        set.active = (set.active + 1).min(nq);
        return QuestionKeyAction::Stay;
    }
    if go_prev {
        set.active = set.active.saturating_sub(1);
        return QuestionKeyAction::Stay;
    }

    // Review screen: 0 = Submit answers, 1 = Cancel.
    if set.active >= nq {
        match code {
            KeyCode::Up => set.review_cursor = 0,
            KeyCode::Down => set.review_cursor = 1,
            KeyCode::Char('1') => return QuestionKeyAction::Submit,
            KeyCode::Char('2') => return QuestionKeyAction::Dismiss,
            KeyCode::Enter => {
                return if set.review_cursor == 0 {
                    QuestionKeyAction::Submit
                } else {
                    QuestionKeyAction::Dismiss
                };
            },
            _ => {},
        }
        return QuestionKeyAction::Stay;
    }

    // A question tab.
    let q_idx = set.active;
    if set.questions[q_idx].is_input() {
        return apply_input_key(set, q_idx, code, mods);
    }
    if set.questions[q_idx].is_rank() {
        return apply_rank_key(set, q_idx, code, mods);
    }
    // Select / MultiSelect.
    let n = set.questions[q_idx].options.len();
    let other_row = set.other_row(q_idx);
    let row_count = set.row_count(q_idx);
    let cursor = set.selections[q_idx].cursor;

    // Text entry into the "Other" row: plain/shifted printables append,
    // Backspace deletes. Other keys fall through to navigation below.
    if cursor == other_row {
        match code {
            KeyCode::Char(c) if !mods.ctrl && !mods.alt => {
                set.selections[q_idx].other_text.push(c);
                return QuestionKeyAction::Stay;
            },
            KeyCode::Backspace => {
                set.selections[q_idx].other_text.pop();
                return QuestionKeyAction::Stay;
            },
            _ => {},
        }
    }

    match code {
        KeyCode::Up => {
            set.selections[q_idx].cursor = cursor.saturating_sub(1);
            QuestionKeyAction::Stay
        },
        KeyCode::Down => {
            set.selections[q_idx].cursor = (cursor + 1).min(row_count.saturating_sub(1));
            QuestionKeyAction::Stay
        },
        // Number keys jump to and act on an option directly (1-based).
        KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => {
            let idx = (c as usize) - ('1' as usize);
            if idx < n {
                set.selections[q_idx].cursor = idx;
                act_on_option(set, q_idx, idx)
            } else {
                QuestionKeyAction::Stay
            }
        },
        KeyCode::Enter | KeyCode::Char(' ') => act_on_row(set, q_idx, cursor),
        _ => QuestionKeyAction::Stay,
    }
}

/// Key handling for an input-kind question (Text/Number/Date/Path): typing
/// edits the value, Number steps with Up/Down, Enter submits when valid.
pub fn apply_input_key(
    set: &mut mermaid_model::question::PendingQuestionSet,
    q_idx: usize,
    code: KeyCode,
    mods: KeyMods,
) -> QuestionKeyAction {
    let is_number = matches!(
        set.questions[q_idx].kind,
        crate::QuestionKind::Number { .. }
    );
    match code {
        KeyCode::Char(c) if !mods.ctrl && !mods.alt => {
            set.selections[q_idx].value.push(c);
            QuestionKeyAction::Stay
        },
        KeyCode::Backspace => {
            set.selections[q_idx].value.pop();
            QuestionKeyAction::Stay
        },
        KeyCode::Up if is_number => {
            step_number(set, q_idx, 1.0);
            QuestionKeyAction::Stay
        },
        KeyCode::Down if is_number => {
            step_number(set, q_idx, -1.0);
            QuestionKeyAction::Stay
        },
        KeyCode::Enter => {
            let kind = set.questions[q_idx].kind.clone();
            if crate::validate_input(&kind, &set.selections[q_idx].value).is_ok() {
                advance_question(set)
            } else {
                QuestionKeyAction::Stay
            }
        },
        _ => QuestionKeyAction::Stay,
    }
}

/// Step a Number question's value by `dir * step`, clamped to min/max.
pub fn step_number(set: &mut mermaid_model::question::PendingQuestionSet, q_idx: usize, dir: f64) {
    let (min, max, step) = match &set.questions[q_idx].kind {
        crate::QuestionKind::Number { min, max, step, .. } => (*min, *max, *step),
        _ => return,
    };
    let step = step.unwrap_or(1.0);
    let cur: f64 = set.selections[q_idx]
        .value
        .trim()
        .parse()
        .unwrap_or(min.unwrap_or(0.0));
    let mut next = cur + dir * step;
    if let Some(lo) = min {
        next = next.max(lo);
    }
    if let Some(hi) = max {
        next = next.min(hi);
    }
    set.selections[q_idx].value = format_number(next);
}

/// Format a number without a trailing `.0` for whole values.
pub fn format_number(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

/// Key handling for a Rank question: Up/Down move the cursor; Space grabs the
/// item under the cursor so Up/Down then moves it; Enter submits the order.
pub fn apply_rank_key(
    set: &mut mermaid_model::question::PendingQuestionSet,
    q_idx: usize,
    code: KeyCode,
    _mods: KeyMods,
) -> QuestionKeyAction {
    let n = set.questions[q_idx].options.len();
    if set.selections[q_idx].order.is_empty() {
        set.selections[q_idx].order = (0..n).collect();
    }
    let sel = &mut set.selections[q_idx];
    match code {
        KeyCode::Char(' ') => {
            sel.grabbed = !sel.grabbed;
            QuestionKeyAction::Stay
        },
        KeyCode::Up => {
            if sel.grabbed && sel.cursor > 0 {
                sel.order.swap(sel.cursor, sel.cursor - 1);
                sel.cursor -= 1;
            } else {
                sel.cursor = sel.cursor.saturating_sub(1);
            }
            QuestionKeyAction::Stay
        },
        KeyCode::Down => {
            if sel.grabbed && sel.cursor + 1 < n {
                sel.order.swap(sel.cursor, sel.cursor + 1);
                sel.cursor += 1;
            } else {
                sel.cursor = (sel.cursor + 1).min(n.saturating_sub(1));
            }
            QuestionKeyAction::Stay
        },
        KeyCode::Enter => advance_question(set),
        _ => QuestionKeyAction::Stay,
    }
}

/// Route a keypress to the front question modal, resolving it into
/// `Cmd::ResolveQuestion` when the user submits or dismisses. Exclusive while a
/// question set is pending.
pub fn handle_question_key(state: &mut State, cmds: &mut Vec<Cmd>, code: KeyCode, mods: KeyMods) {
    let action = {
        let Some(set) = state.pending_question.front_mut() else {
            return;
        };
        apply_question_key(set, code, mods)
    };
    let resolution = match action {
        QuestionKeyAction::Stay => return,
        QuestionKeyAction::Submit => {
            let Some(set) = state.pending_question.front() else {
                return;
            };
            crate::QuestionResolution::Answered {
                answers: set.build_answers(),
                remember: set.remember,
            }
        },
        QuestionKeyAction::Dismiss => crate::QuestionResolution::Dismissed,
        QuestionKeyAction::Reformulate => crate::QuestionResolution::Reformulate,
    };
    if let Some(front) = state.pending_question.front() {
        let call_id = front.call_id;
        state.pending_question.pop_front();
        cmds.push(Cmd::ResolveQuestion {
            call_id,
            resolution,
        });
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "predates the lint; see .github/baselines/expect_budget.txt"
)]
pub fn handle_key(state: &mut State, cmds: &mut Vec<Cmd>, code: KeyCode, mods: KeyMods) {
    // Ctrl+C: press twice to exit. The first press does the useful thing —
    // interrupts a running turn (like Esc) or clears typed input — and arms a
    // short confirm window; a second press inside the window exits. This also
    // makes a stray copy-chord harmless: on terminals without the kitty
    // protocol Ctrl+Shift+C arrives byte-identical to Ctrl+C, and with the
    // protocol it arrives with the SHIFT bit — blocked here by `!mods.shift`
    // and intercepted earlier in the run loop as the copy action. Ctrl+D on
    // empty input and `/quit` remain immediate exits.
    if mods.ctrl && !mods.shift && code == KeyCode::Char('c') {
        if state
            .ui
            .exit_armed_until
            .is_some_and(|deadline| state.now <= deadline)
        {
            request_exit(state, cmds);
            return;
        }
        if state.is_busy() {
            handle_cancel_turn(state, cmds);
        } else if !state.ui.input_buffer.is_empty() {
            state.ui.input_buffer.clear();
            state.ui.input_cursor = 0;
            state.ui.palette_cursor = None;
            state.ui.input_history_cursor = None;
            state.ui.history_draft.clear();
        }
        state.ui.exit_armed_until = Some(
            state.now
                + chrono::Duration::seconds(mermaid_model::constants::UI_EXIT_CONFIRM_WINDOW_SECS),
        );
        return;
    }
    // Any other key disarms a pending exit confirmation — the user moved on.
    state.ui.exit_armed_until = None;
    // Same for the double-Esc rewind arming: only Esc keeps it alive.
    if code != KeyCode::Escape {
        state.ui.esc_armed_at = None;
    }

    // Ctrl+B: send a running foreground command to the background (it keeps
    // running as a `/processes` entry) instead of waiting on it. Only
    // meaningful while tools are executing; a swallowed no-op otherwise.
    if mods.ctrl && code == KeyCode::Char('b') {
        if let TurnState::ExecutingTools { id, .. } = &state.turn {
            cmds.push(Cmd::BackgroundScope(*id));
        }
        return;
    }

    // Ctrl+L: force a full repaint (the universal readline "redraw screen"
    // chord). Recovers from anything that scribbled on the terminal behind
    // ratatui's back buffer. Meta-level like Ctrl+C/Ctrl+B — deliberately
    // above the modal handlers so a repaint works with a modal open too.
    if mods.ctrl && code == KeyCode::Char('l') {
        state.ui.full_redraw_seq = state.ui.full_redraw_seq.wrapping_add(1);
        return;
    }

    // Ctrl+T: toggle the task checklist between its expanded rows and the
    // one-line collapsed form. Meta-level like Ctrl+L — works everywhere,
    // no-op in effect when no checklist is showing.
    if mods.ctrl && code == KeyCode::Char('t') {
        state.ui.tasks_collapsed = !state.ui.tasks_collapsed;
        return;
    }

    // Ctrl+O: compose the input draft in $VISUAL/$EDITOR. Allowed while a
    // turn is busy (it only edits the draft), but only from the plain input
    // surface — never over a picker or a pending approval/question modal,
    // whose keyboard focus it would steal.
    if mods.ctrl
        && code == KeyCode::Char('o')
        && matches!(state.ui.mode, UiMode::EditingInput)
        && state.pending_approval.is_empty()
        && state.pending_question.is_empty()
        && state.confirm.is_none()
    {
        cmds.push(Cmd::ComposeInEditor {
            text: state.ui.input_buffer.clone(),
        });
        return;
    }

    // Transcript scrolling (keyboard): PageUp/PageDown by a page, Shift+Up/Down
    // by a line, End to jump back to the newest message. Reuses the pure
    // publish-then-diff scroll pipeline — the render layer applies the delta,
    // so the reducer only bumps a counter. Positive accum scrolls toward older
    // messages (matches `Msg::MouseScroll`).
    const SCROLL_PAGE: i32 = 10;
    match code {
        KeyCode::PageUp => {
            state.ui.mouse_scroll_accum = state.ui.mouse_scroll_accum.saturating_add(SCROLL_PAGE);
            return;
        },
        KeyCode::PageDown => {
            state.ui.mouse_scroll_accum = state.ui.mouse_scroll_accum.saturating_sub(SCROLL_PAGE);
            return;
        },
        KeyCode::Up if mods.shift => {
            state.ui.mouse_scroll_accum = state.ui.mouse_scroll_accum.saturating_add(1);
            return;
        },
        KeyCode::Down if mods.shift => {
            state.ui.mouse_scroll_accum = state.ui.mouse_scroll_accum.saturating_sub(1);
            return;
        },
        KeyCode::End => {
            state.ui.scroll_to_bottom_seq = state.ui.scroll_to_bottom_seq.wrapping_add(1);
            return;
        },
        _ => {},
    }

    // Inline approval modal: while a tool awaits approval the prompt is
    // exclusive. Direct keys resolve immediately — 1/y approve · 2/a approve +
    // don't-ask-again · 3/n/Esc deny. Or move the highlight with ↑/↓ and press
    // Enter on it. Sits ABOVE the Esc-cancel guard so Esc denies just this tool
    // (keeping the turn alive) rather than cancelling the whole turn. Any other
    // key is swallowed. Resolving emits `Cmd::ResolveApproval`, which unblocks
    // the parked tool task via the broker.
    match state.focus() {
        Focus::ApprovalModal => {
            handle_approval_key(state, cmds, code);
            return;
        },
        Focus::QuestionModal => {
            handle_question_key(state, cmds, code, mods);
            return;
        },
        Focus::ConfirmModal => {
            handle_confirm_key(state, cmds, code);
            return;
        },
        // Pickers dispatch BELOW the busy-Esc guard and the composer
        // chords (Ctrl+D still quits, Alt+T still cycles, with a picker
        // open) — same order the guard chain always had.
        Focus::Picker | Focus::Composer => {},
    }

    // Esc interrupts active work by cancelling the current turn. It must NEVER
    // exit mermaid — only Ctrl+C (or `/quit`) does that. A second Esc while the
    // turn is already cancelling is a no-op: the cancellation is underway, and
    // Ctrl+C is the escalation path if it ever wedges. (Previously a second Esc
    // mid-cancel force-exited, which booted users out unexpectedly and could
    // leave a backgrounded process holding the terminal.) When idle, Esc falls
    // through to the palette/input/focus handlers below.
    if mods.is_empty() && code == KeyCode::Escape && state.is_busy() {
        if !matches!(state.turn, TurnState::Cancelling { .. }) {
            handle_cancel_turn(state, cmds);
        }
        return;
    }

    // Ctrl+D on empty input quits.
    if mods.ctrl && code == KeyCode::Char('d') && state.ui.input_buffer.is_empty() {
        request_exit(state, cmds);
        return;
    }

    // Ctrl+V: read the system clipboard and paste its contents. Gate
    // on `EditingInput` + no confirmation modal so the palette and
    // conversation-list picker don't swallow the keystroke. The
    // actual clipboard read happens off-thread in the effect runner
    // (xclip / wl-paste / pngpaste / PowerShell can block for
    // hundreds of ms on macOS); result comes back asynchronously as
    // `Msg::ClipboardRead(Image|Text|Empty|Error)`.
    if mods.ctrl
        && code == KeyCode::Char('v')
        && matches!(state.ui.mode, UiMode::EditingInput)
        && state.confirm.is_none()
    {
        // Mark a read in flight so a fast Enter waits for it (paste-race guard).
        state.ui.clipboard_reads_pending += 1;
        cmds.push(Cmd::ReadClipboard);
        return;
    }

    // Ctrl+J: insert a newline at the cursor (multi-line input). Works on
    // legacy terminals too — raw-mode crossterm parses the LF byte Ctrl+J
    // sends as `Char('j') + CONTROL` while Enter arrives as CR — so the
    // chord needs no kitty disambiguation. Gated like Ctrl+V so pickers
    // and modals never receive a stray newline.
    if mods.ctrl
        && code == KeyCode::Char('j')
        && matches!(state.ui.mode, UiMode::EditingInput)
        && state.confirm.is_none()
    {
        insert_text_at_cursor(state, cmds, "\n");
        return;
    }

    // Alt+T cycles reasoning depth. Persists per-model so cycling on
    // Sonnet doesn't bleed into the next session with Ollama.
    if mods.alt && code == KeyCode::Char('t') {
        let next = cycle_reasoning(state.session.reasoning);
        state.session.reasoning = next;
        cmds.push(Cmd::PersistReasoningFor {
            model_id: state.session.model_id.clone(),
            level: next,
        });
        // The bottom status bar already shows the new reasoning level — no banner.
        return;
    }

    // Shift+Tab cycles the safety mode (plan → read-only → ask → auto →
    // full-access). Session-scoped: the `[safety]` config value stays the
    // persistent default, so a session never silently inherits a more-permissive
    // mode from a previous run. Mirrors the Alt+T reasoning cycle above.
    if code == KeyCode::BackTab {
        apply_safety_mode(state, cmds, cycle_safety(state.session.safety_mode));
        // The bottom status bar already shows the new safety mode — no banner.
        return;
    }

    // A picker owns the keystroke from here down (the shared navigation
    // core lives in `picker::picker_step`; each handler keeps only its
    // confirm semantics and extra keys).
    if state.focus() == Focus::Picker {
        handle_picker_key(state, cmds, code);
        return;
    }

    // Slash-palette navigation — intercepts ↑/↓/Tab/Esc while the
    // input buffer opens with `/`. Enter falls through to the normal
    // handler below so the command actually dispatches.
    if state.ui.input_buffer.starts_with('/') {
        use crate::slash_commands::filter_entries;
        let typed = state
            .ui
            .input_buffer
            .trim_start_matches('/')
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_string();
        let candidates = filter_entries(&typed, &state.plugin_commands);
        match code {
            KeyCode::Up => {
                let cur = state.ui.palette_cursor.unwrap_or(0);
                state.ui.palette_cursor = Some(cur.saturating_sub(1));
                return;
            },
            KeyCode::Down => {
                let max = candidates.len().saturating_sub(1);
                let cur = state.ui.palette_cursor.unwrap_or(0);
                state.ui.palette_cursor = Some((cur + 1).min(max));
                return;
            },
            KeyCode::Tab => {
                let sel = state.ui.palette_cursor.unwrap_or(0);
                if let Some(entry) = candidates.get(sel) {
                    let completed = format!("/{} ", entry.name());
                    drop(candidates);
                    state.ui.input_buffer = completed;
                    state.ui.input_cursor = state.ui.input_buffer.len();
                    state.ui.palette_cursor = Some(0);
                }
                return;
            },
            KeyCode::Escape => {
                state.ui.input_buffer.clear();
                state.ui.input_cursor = 0;
                state.ui.palette_cursor = None;
                return;
            },
            KeyCode::Enter if !mods.shift => {
                // Complete-then-execute: replace the command word with
                // the highlighted candidate (preserving any args the
                // user already typed), then fall through to the Enter
                // handler below so the command actually dispatches.
                let sel = state.ui.palette_cursor.unwrap_or(0);
                if let Some(entry) = candidates.get(sel) {
                    let name = entry.name().to_string();
                    drop(candidates);
                    let raw = state.ui.input_buffer.clone();
                    let after_slash = raw.trim_start_matches('/');
                    let rest = match after_slash.find(char::is_whitespace) {
                        Some(idx) => &after_slash[idx..],
                        None => "",
                    };
                    state.ui.input_buffer = format!("/{name}{rest}");
                    state.ui.input_cursor = state.ui.input_buffer.len();
                }
                // Fall through to the Enter handler below.
            },
            _ => {
                // Fall through to normal key handling (char/Backspace
                // update the filter; palette_cursor gets reset below).
            },
        }
    }

    // @-mention file picker — intercepts ↑/↓/Tab/Enter/Esc while an
    // @-token is under the cursor (never on a slash command; the palette
    // above owns that surface). Enter COMPLETES here instead of submitting:
    // picking a file and firing the prompt with one keypress would send a
    // half-written message.
    if state.ui.file_picker_open() {
        match code {
            KeyCode::Up => {
                let cur = state.ui.file_picker_cursor.unwrap_or(0);
                state.ui.file_picker_cursor = Some(cur.saturating_sub(1));
                return;
            },
            KeyCode::Down => {
                let max = state.ui.file_picker_matches.len().saturating_sub(1);
                let cur = state.ui.file_picker_cursor.unwrap_or(0);
                state.ui.file_picker_cursor = Some((cur + 1).min(max));
                return;
            },
            KeyCode::Tab => {
                complete_file_mention(state);
                return;
            },
            KeyCode::Enter if !mods.shift && !state.ui.file_picker_matches.is_empty() => {
                complete_file_mention(state);
                return;
            },
            KeyCode::Escape => {
                // Dismiss for THIS token only; the input stays untouched
                // (deliberate divergence from the slash palette's clear-all —
                // the @-text is prose the user typed, not a command filter).
                state.ui.file_picker_dismissed = true;
                state.ui.file_picker_matches.clear();
                state.ui.file_picker_cursor = None;
                return;
            },
            _ => {
                // Fall through: chars/Backspace edit the query below and the
                // trailing refresh re-ranks the matches.
            },
        }
    }

    // Enter submits the current input (or triggers the slash palette
    // pick) regardless of shift — Ctrl+J is the newline chord for
    // multi-line input. This arm enqueues a synthetic `Msg` on
    // `pending_msgs` rather than invoking the dispatch directly — the
    // outer `update()` drain will run the follow-up with stale-filter +
    // pending-msgs guarantees intact.
    if code == KeyCode::Enter {
        // Paste-race guard: if a Ctrl+V clipboard read is still in flight, hold
        // the submit until it lands. `handle_clipboard_read` re-runs
        // `submit_current_input` once the last pending read drains, re-deriving
        // the text + attachments so a just-pasted image is included rather than
        // dropped (and no stray `[Image #N]` leaks into the next prompt).
        if state.ui.clipboard_reads_pending > 0 {
            if !state.ui.input_buffer.trim().is_empty() {
                state.ui.submit_after_clipboard = true;
            }
            return;
        }
        submit_current_input(state);
        return;
    }

    if mods.is_empty() || mods.shift {
        match code {
            KeyCode::Char(c) => {
                // Any text mutation resets history nav — the user's
                // typing wins over whatever historical entry was
                // on-screen. It also un-dismisses the @-mention picker:
                // Esc dismisses per-token, typing reopens.
                state.ui.input_history_cursor = None;
                state.ui.history_draft.clear();
                state.ui.file_picker_dismissed = false;
                let pos = clamp_cursor(&state.ui.input_buffer, state.ui.input_cursor);
                state.ui.input_buffer.insert(pos, c);
                state.ui.input_cursor = clamp_cursor(&state.ui.input_buffer, pos + c.len_utf8());
                // Opening the palette, or editing its filter, resets
                // the cursor to the first candidate — stops stale
                // indices from pointing past the end of a shrinking
                // filter result.
                if state.ui.input_buffer.starts_with('/') {
                    state.ui.palette_cursor = Some(0);
                }
            },
            KeyCode::Backspace => {
                state.ui.input_history_cursor = None;
                state.ui.history_draft.clear();
                state.ui.file_picker_dismissed = false;
                let pos = clamp_cursor(&state.ui.input_buffer, state.ui.input_cursor);
                // If a whole `[Image #N]` pill ends at the cursor, delete it and
                // drop its attachment together; otherwise one codepoint.
                if let Some((start, number)) =
                    crate::image_token::token_ending_at(&state.ui.input_buffer, pos)
                {
                    state.ui.input_buffer.drain(start..pos);
                    state.ui.input_cursor = start;
                    state.ui.attachments.retain(|a| a.number != number);
                } else if pos > 0 {
                    let new_pos = state.ui.input_buffer.floor_char_boundary(pos - 1);
                    state.ui.input_buffer.drain(new_pos..pos);
                    state.ui.input_cursor = new_pos;
                }
                if state.ui.input_buffer.starts_with('/') {
                    state.ui.palette_cursor = Some(0);
                } else {
                    state.ui.palette_cursor = None;
                }
            },
            KeyCode::Delete => {
                state.ui.input_history_cursor = None;
                state.ui.history_draft.clear();
                state.ui.file_picker_dismissed = false;
                let pos = clamp_cursor(&state.ui.input_buffer, state.ui.input_cursor);
                // Symmetric to Backspace: a pill starting at the cursor deletes
                // whole, taking its attachment with it.
                if let Some((end, number)) =
                    crate::image_token::token_starting_at(&state.ui.input_buffer, pos)
                {
                    state.ui.input_buffer.drain(pos..end);
                    state.ui.attachments.retain(|a| a.number != number);
                } else if pos < state.ui.input_buffer.len() {
                    let next = state.ui.input_buffer.ceil_char_boundary(pos + 1);
                    state.ui.input_buffer.drain(pos..next);
                }
                if state.ui.input_buffer.starts_with('/') {
                    state.ui.palette_cursor = Some(0);
                } else {
                    state.ui.palette_cursor = None;
                }
            },
            KeyCode::Left => {
                let pos = clamp_cursor(&state.ui.input_buffer, state.ui.input_cursor);
                if pos > 0 {
                    state.ui.input_cursor = state.ui.input_buffer.floor_char_boundary(pos - 1);
                }
            },
            KeyCode::Right => {
                let pos = clamp_cursor(&state.ui.input_buffer, state.ui.input_cursor);
                if pos < state.ui.input_buffer.len() {
                    state.ui.input_cursor = state.ui.input_buffer.ceil_char_boundary(pos + 1);
                }
            },
            KeyCode::Home => state.ui.input_cursor = 0,
            KeyCode::End => state.ui.input_cursor = state.ui.input_buffer.len(),
            KeyCode::Up => {
                // Images are inline `[Image #N]` tokens now, so Up always steps
                // back through input history — no attachment-bar contention.
                history_nav_back(state);
            },
            KeyCode::Down => {
                history_nav_forward(state);
            },
            KeyCode::Escape => {
                // Clear any in-progress history nav, then run the double-Esc
                // rewind arming: first idle Esc arms, a second within the
                // window opens the rewind picker. Busy Esc never reaches here
                // (it cancelled and returned above).
                state.ui.input_history_cursor = None;
                state.ui.history_draft.clear();
                handle_rewind_esc(state);
            },
            _ => {},
        }
        // The buffer or cursor may have moved: re-evaluate the @-mention
        // token, re-rank matches, and fire the project walk on open.
        refresh_file_picker(state, cmds);
    }
}

/// Double-Esc window: a second idle Esc within this many ms of the first
/// opens the rewind picker.
pub const ESC_REWIND_WINDOW_MS: i64 = 1000;

/// Idle-Esc arming: first press arms, a second within the window fires the
/// rewind picker; past the window the press re-arms. Compared against the
/// injected `state.now` (pure; replay-exact).
pub fn handle_rewind_esc(state: &mut State) {
    let fired = state.ui.esc_armed_at.is_some_and(|armed| {
        (state.now - armed) <= chrono::Duration::milliseconds(ESC_REWIND_WINDOW_MS)
    });
    if !fired {
        state.ui.esc_armed_at = Some(state.now);
        return;
    }
    state.ui.esc_armed_at = None;
    let candidates = rewind_candidates(state.session.messages());
    if candidates.is_empty() {
        // Nothing to rewind to — silently no-op.
        return;
    }
    state.ui.mode = UiMode::RewindPicker {
        candidates,
        cursor: 0,
    };
}

/// The rewind targets: user-role `Normal` messages, NEWEST FIRST (the most
/// recent exchange is the most likely rewind point). Excerpts are the first
/// non-empty line, clipped.
pub fn rewind_candidates(messages: &[ChatMessage]) -> Vec<crate::RewindCandidate> {
    const EXCERPT_MAX_CHARS: usize = 80;
    messages
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, m)| {
            m.role == mermaid_model::models::MessageRole::User
                && m.kind == mermaid_model::models::ChatMessageKind::Normal
        })
        .map(|(message_index, m)| {
            let line = m
                .content
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .unwrap_or("(empty message)");
            let excerpt = if line.chars().count() > EXCERPT_MAX_CHARS {
                let mut clipped: String = line.chars().take(EXCERPT_MAX_CHARS).collect();
                clipped.push('…');
                clipped
            } else {
                line.to_string()
            };
            crate::RewindCandidate {
                message_index,
                excerpt,
            }
        })
        .collect()
}

/// Handle keyboard input while the rewind picker is open. Up/Down walk the
/// candidate list; Enter forks the session at the highlighted user message;
/// Esc dismisses without touching the conversation.
/// Route a keystroke to whichever `UiMode` picker is open.
///
/// Reached only when [`Focus::Picker`] resolved, so the per-handler mode
/// destructures are re-checks, not policy.
pub fn handle_picker_key(state: &mut State, cmds: &mut Vec<Cmd>, code: KeyCode) {
    match state.ui.mode {
        UiMode::ModelPicker { .. } => handle_model_picker_key(state, cmds, code),
        UiMode::ConversationList { .. } => handle_conversation_list_key(state, cmds, code),
        UiMode::RewindPicker { .. } => handle_rewind_picker_key(state, cmds, code),
        UiMode::PlanConfig { .. } => handle_plan_config_key(state, cmds, code),
        UiMode::EditingInput | UiMode::ModelList => {},
    }
}

/// The yes/no confirmation modal (`/clear`).
///
/// y/Enter accepts, n/Esc declines. Extracted verbatim from the old guard
/// chain.
pub fn handle_confirm_key(state: &mut State, cmds: &mut Vec<Cmd>, code: KeyCode) {
    match code {
        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
            handle_confirm_accepted(state, cmds);
        },
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Escape => {
            state.confirm = None;
        },
        _ => {},
    }
}

/// The inline tool-approval modal.
///
/// Exclusive while a tool awaits approval: 1/y approve, 2/a approve-always
/// (when allowlistable), 3/n/Esc deny, or highlight with the arrows and
/// Enter. Body extracted verbatim from the old guard chain;
/// `Focus::ApprovalModal` is the routing authority now.
pub fn handle_approval_key(state: &mut State, cmds: &mut Vec<Cmd>, code: KeyCode) {
    {
        use crate::ApprovalChoice;
        // Content-bearing external tools are non-allowlistable: the gate signals
        // this with an empty allowlist scope, and the modal then omits the
        // middle "approve always" option (#6, #31). Layout:
        //   allowlistable:     0 = Yes, 1 = Yes-always, 2 = No
        //   non-allowlistable: 0 = Yes,                 1 = No
        let allowlistable = state
            .pending_approval
            .front()
            .map(|i| !i.allowlist_scope.is_empty())
            .unwrap_or(false);
        let option_count = if allowlistable { 3 } else { 2 };
        let choice_for = |idx: usize| match (allowlistable, idx) {
            (true, 0) | (false, 0) => ApprovalChoice::Approve,
            (true, 1) => ApprovalChoice::ApproveAlways,
            _ => ApprovalChoice::Deny,
        };
        // Copy the current highlight out so the ↑/↓ arms can take a fresh
        // mutable borrow without conflicting.
        let selected = state
            .pending_approval
            .front()
            .map(|i| i.selected_option)
            .unwrap_or(0);
        let choice = match code {
            KeyCode::Char('1') | KeyCode::Char('y') | KeyCode::Char('Y') => {
                Some(ApprovalChoice::Approve)
            },
            // 'a'/'A' and '2' select approve-always only when allowlistable;
            // when not, '2' is the (second, final) "No" option.
            KeyCode::Char('a') | KeyCode::Char('A') if allowlistable => {
                Some(ApprovalChoice::ApproveAlways)
            },
            KeyCode::Char('2') => Some(if allowlistable {
                ApprovalChoice::ApproveAlways
            } else {
                ApprovalChoice::Deny
            }),
            KeyCode::Char('3') | KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Escape => {
                Some(ApprovalChoice::Deny)
            },
            KeyCode::Enter => Some(choice_for(selected)),
            KeyCode::Up => {
                if let Some(front) = state.pending_approval.front_mut() {
                    front.selected_option = selected.saturating_sub(1);
                }
                None
            },
            KeyCode::Down => {
                if let Some(front) = state.pending_approval.front_mut() {
                    front.selected_option = (selected + 1).min(option_count - 1);
                }
                None
            },
            _ => None,
        };
        if let Some(decision) = choice
            && let Some(call_id) = state.pending_approval.front().map(|i| i.call_id)
        {
            state.pending_approval.pop_front();
            cmds.push(Cmd::ResolveApproval { call_id, decision });
        }
    }
}

pub fn handle_rewind_picker_key(state: &mut State, cmds: &mut Vec<Cmd>, code: KeyCode) {
    let UiMode::RewindPicker {
        ref candidates,
        ref mut cursor,
    } = state.ui.mode
    else {
        return;
    };
    match picker_step(code, cursor, candidates.len()) {
        PickerStep::Confirm(row) => {
            if let Some(candidate) = candidates.get(row) {
                let message_index = candidate.message_index;
                fork_conversation_at(state, cmds, message_index);
            }
        },
        PickerStep::Dismiss => {
            state.ui.mode = UiMode::EditingInput;
        },
        PickerStep::Moved | PickerStep::Other => {},
    }
}

pub fn fork_conversation_at(state: &mut State, cmds: &mut Vec<Cmd>, message_index: usize) {
    // 1. Persist the original FIRST — different id, different file, so this
    //    can never clobber the fork's save below.
    cmds.push(state.session.save_conversation_cmd());

    // File checkpoints anchored past the cut belong to the timeline being
    // discarded; ask the runtime store which exist (async — the reply arm
    // emits a /restore hint). Uses the ORIGINAL id: the fork's history is a
    // strict prefix, so anchors always reference the original session.
    cmds.push(Cmd::Query(Query::ListForkCheckpoints {
        session_id: state.session.conversation.id.clone(),
        message_index,
    }));

    let original = &state.session.conversation;
    let original_id = original.id.clone();
    // 2. Mint the fork from the injected clock — the new id is a pure
    //    function of `state.now` (replay-exact; the `/clear` handler is the
    //    precedent).
    let mut fork = crate::ConversationHistory::new(
        original.project_path.clone(),
        original.model_name.clone(),
        state.now,
    );
    // Ids are millisecond-derived; a fork minted in the SAME millisecond as
    // the original would share its id — and its save file. Deterministic
    // 1ms bump keeps the fork pure while guaranteeing a distinct file.
    if fork.id == original.id {
        fork = crate::ConversationHistory::new(
            original.project_path.clone(),
            original.model_name.clone(),
            state.now + chrono::Duration::milliseconds(1),
        );
    }
    fork.title = original.title.clone();
    // The cut lands on a user message, which always starts a run, so the
    // prefix can't split a tool_use/tool_result pair — normalize anyway as
    // defense in depth.
    let mut messages: Vec<ChatMessage> = original.messages()[..message_index].to_vec();
    crate::compaction::normalize_history(&mut messages);
    fork.set_messages(messages);
    fork.input_history = original.input_history.clone();
    fork.git_branch = original.git_branch.clone();
    fork.git_sha = original.git_sha.clone();
    fork.cli_version = original.cli_version.clone();
    // Carried for provenance: the records' archive paths point at the
    // ORIGINAL session's archives (read-only history, never rewritten).
    fork.compactions = original.compactions.clone();
    // The payoff: the resume picker's "forked" chip and the `.meta` sidecar
    // light up from these two fields with zero render changes.
    fork.forked_from = Some(original_id.clone());
    fork.parent_session = Some(original_id);
    let selected = original.messages().get(message_index).cloned();

    // 3. Swap. Cumulative token meters continue (same spend, same session of
    //    work); last-usage described a model call on the dropped suffix, so it
    //    has no successor here — reset.
    // The fork starts with an empty checklist (a rewound plan describes
    // dropped work); the broker must forget it too or the next task tool
    // call would republish the stale list.
    state.session.replace_conversation(fork);
    state.session.last_token_usage = None;
    // The context gauge is RE-ESTIMATED, not cleared. Dropping it to `None`
    // rendered "context: n/a" — which reads as "unknown", when in fact the
    // fork's context is the most precisely known thing about it: the prefix we
    // just chose. The gauge going 250k → n/a after a rewind looked like the
    // meter had broken; it should go 250k → whatever the prefix costs.
    // Marked as an estimate (the `~` prefix) until the next real call returns
    // provider-counted usage.
    state.session.context_usage = Some(estimate_current_context(state));
    cmds.push(Cmd::SyncTaskStore(crate::ChecklistStore::default()));
    // The fork minted a fresh conversation id, so it gets its own scratch
    // dir too — the original session's scratch contents describe work on
    // the timeline being discarded.
    refresh_scratchpad(state, cmds);

    // 4. `state.ids.image` is NOT re-based: the allocator stays monotonic, so
    //    image numbers remain unique across the fork (seed_conversation's
    //    re-base is for fresh processes only).

    // 5. Pre-fill the composer with the selected message. The pre-rewind
    //    draft and its staged attachments are deliberately dropped; the
    //    selected message's own images are RE-STAGED so its `[Image #N]`
    //    tokens resolve at submit.
    state.ui.attachments.clear();
    if let Some(msg) = selected {
        state.ui.input_buffer = msg.content.clone();
        state.ui.input_cursor = state.ui.input_buffer.len();
        if let (Some(images), Some(numbers)) = (&msg.images, &msg.image_numbers) {
            for (b64, number) in images.iter().zip(numbers) {
                restage_image(state, cmds, b64, *number);
            }
        }
    }

    // 6. Close the picker and save the fork. Edge: rewinding to the FIRST
    //    user message yields an empty prefix — `save_conversation` skips
    //    empty conversations, so the fork file materializes on first resend.
    state.ui.mode = UiMode::EditingInput;
    state.ui.esc_armed_at = None;
    cmds.push(state.session.save_conversation_cmd());
    emit_title_if_changed(state, cmds);
}

/// Re-stage one of the selected message's images as a live attachment so its
/// inline `[Image #N]` token resolves at submit. Keeps the ORIGINAL number
/// (the token in the pre-filled text references it). A base64 decode failure
/// skips the attachment — the token degrades to literal text, matching how
/// orphan tokens behave today. The stored format isn't recorded per-image;
/// "png" is the decode-side fallback (viewers sniff the real magic bytes).
pub fn restage_image(state: &mut State, cmds: &mut Vec<Cmd>, base64_data: &str, number: u64) {
    let Ok(bytes) = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, base64_data)
    else {
        return;
    };
    let id = state.ids.tool_call.next();
    let format = "png".to_string();
    let temp_path = state.temp_dir.join(format!("mermaid-img-{id}.{format}"));
    state.ui.attachments.push(crate::state::Attachment {
        id,
        number,
        base64_data: base64_data.to_string(),
        temp_path: temp_path.clone(),
        size_bytes: bytes.len(),
        format: format.clone(),
    });
    cmds.push(Cmd::WriteImageToTemp {
        path: temp_path,
        bytes,
        format,
    });
}

/// The conversation id just changed (`/clear`, `/load`, rewind fork): drop
/// the old session's scratch dir handle and ask the effect layer to
/// materialize one for the new id. Pure — the directory creation happens in
/// the `EnsureScratchpad` handler, and `Msg::ScratchpadReady` stamps the
/// path back onto the session.
pub fn refresh_scratchpad(state: &mut State, cmds: &mut Vec<Cmd>) {
    state.session.scratchpad = None;
    cmds.push(Cmd::EnsureScratchpad {
        session_id: state.session.conversation.id.clone(),
    });
}

/// Switch the session model. The one path — `/model <id>` and the `/model`
/// picker both land here, so a model chosen from the list gets the same
/// vision re-probe, persistence, and Ollama pull as one typed by hand.
pub fn switch_model(state: &mut State, cmds: &mut Vec<Cmd>, new_model: String) {
    let pull_target = ollama_pull_target(&new_model);
    state.session.model_id = new_model.clone();
    state.runtime.set_model(&new_model);
    // Refresh vision capability for the newly-selected model (set_model
    // reset the snapshot to a static default). Nag only if an image is
    // already staged — i.e. you switched TO a no-vision model with a
    // pending paste; otherwise this just keeps `/doctor` honest.
    cmds.push(Cmd::ProbeVision {
        model_id: state.session.model_id.clone(),
        warn: !state.ui.attachments.is_empty(),
    });
    // The bottom status bar shows the new model — no banner.
    cmds.push(Cmd::PersistLastModel(new_model));
    if let Some(model) = pull_target {
        cmds.push(Cmd::PullOllamaModel { model });
    }
}

/// Rows of the `/model` picker matching `query`, in list order.
///
/// Subsequence match over the model id, case-insensitive: typing `oplus` finds
/// `anthropic/claude-opus-4-5`. A provider can return 200 ids, so filtering is
/// what makes the list usable at all — the difference between this picker and
/// the fixed four-row ones it is modeled on.
#[must_use]
pub fn filter_model_choices<'a>(
    candidates: &'a [crate::state::ModelChoice],
    query: &str,
) -> Vec<&'a crate::state::ModelChoice> {
    let needle = query.trim().to_lowercase();
    candidates
        .iter()
        .filter(|c| needle.is_empty() || is_subsequence(&needle, &c.id.to_lowercase()))
        .collect()
}

/// Are `needle`'s chars present in `haystack`, in order (not necessarily
/// adjacent)? Both are expected lowercase.
pub fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut chars = haystack.chars();
    needle.chars().all(|n| chars.any(|h| h == n))
}

/// Handle keyboard input while the `/model` picker is open. ↑/↓ walk the
/// filtered rows, Enter switches, Esc dismisses, and printable characters
/// (plus Backspace) edit the filter — the input buffer is untouched, so
/// dismissing restores whatever draft was there.
pub fn handle_model_picker_key(state: &mut State, cmds: &mut Vec<Cmd>, code: KeyCode) {
    let UiMode::ModelPicker {
        ref candidates,
        ref mut query,
        ref mut cursor,
        ..
    } = state.ui.mode
    else {
        return;
    };
    // The cursor walks the FILTERED list, so the query decides the length.
    let filtered_len = filter_model_choices(candidates, query).len();
    match picker_step(code, cursor, filtered_len) {
        PickerStep::Confirm(row) => {
            let chosen = filter_model_choices(candidates, query)
                .get(row)
                .map(|c| c.id.clone());
            state.ui.mode = UiMode::EditingInput;
            if let Some(id) = chosen {
                switch_model(state, cmds, id);
            }
        },
        PickerStep::Dismiss => state.ui.mode = UiMode::EditingInput,
        PickerStep::Moved => {},
        PickerStep::Other => match code {
            KeyCode::Backspace => {
                query.pop();
                *cursor = 0;
            },
            KeyCode::Char(c) => {
                query.push(c);
                // Narrowing invalidates the old row position; start from the top.
                *cursor = 0;
            },
            _ => {},
        },
    }
}

/// Handle keyboard input while the conversation-list picker is open.
/// Up/Down walk the cursor within the candidate list; Enter loads the
/// highlighted session; Esc dismisses.
pub fn handle_conversation_list_key(state: &mut State, cmds: &mut Vec<Cmd>, code: KeyCode) {
    let UiMode::ConversationList {
        ref candidates,
        ref mut cursor,
    } = state.ui.mode
    else {
        return;
    };
    match picker_step(code, cursor, candidates.len()) {
        PickerStep::Confirm(row) => {
            if let Some(summary) = candidates.get(row) {
                cmds.push(Cmd::Query(Query::LoadConversation {
                    id: summary.id.clone(),
                }));
            }
            // Mode flips on `QueryResult::ConversationLoaded` — leave as-is
            // until then so the user sees the list until the load
            // completes.
        },
        PickerStep::Dismiss => {
            state.ui.mode = UiMode::EditingInput;
        },
        PickerStep::Moved | PickerStep::Other => {},
    }
}

/// Cap on ranked matches held in state (and shown by the widget's window).
pub const FILE_PICKER_MAX_MATCHES: usize = 50;

/// Re-rank `file_picker_matches` for the active @-token against the cached
/// project file list. Pure recompute — never fires a walk.
pub fn recompute_file_matches(state: &mut State) {
    let Some(token) = state.ui.active_file_token() else {
        state.ui.file_picker_matches.clear();
        state.ui.file_picker_cursor = None;
        return;
    };
    let query = state.ui.input_buffer[token.query_start..token.query_end].to_string();
    let files: &[String] = state.ui.project_files.as_deref().unwrap_or(&[]);
    state.ui.file_picker_matches =
        crate::file_mention::fuzzy_rank(files, &query, FILE_PICKER_MAX_MATCHES);
    // Clamp (don't reset) the cursor so ↑/↓ position survives narrowing.
    let max = state.ui.file_picker_matches.len().saturating_sub(1);
    let cur = state.ui.file_picker_cursor.unwrap_or(0);
    state.ui.file_picker_cursor = Some(cur.min(max));
}

/// Token re-evaluation after a text mutation or cursor move: rank matches,
/// and on the CLOSED → OPEN transition fire a fresh project walk
/// (stale-while-revalidate — the user filters the cached list instantly and
/// the fresh list swaps in via `QueryResult::ProjectFilesListed`). The in-flight
/// flag dedupes: reopening while a walk runs never spawns a second one.
pub fn refresh_file_picker(state: &mut State, cmds: &mut Vec<Cmd>) {
    let was_open = state.ui.file_picker_cursor.is_some();
    recompute_file_matches(state);
    let is_open = state.ui.file_picker_cursor.is_some();
    if is_open && !was_open && !state.ui.project_files_loading {
        state.ui.project_files_loading = true;
        cmds.push(Cmd::Query(Query::ListProjectFiles));
    }
}

/// Complete the active @-token with the highlighted match: splice
/// `@<path> ` over `@<query>` and land the cursor after the space. The
/// trailing space closes the token, so the picker drops on its own.
pub fn complete_file_mention(state: &mut State) {
    let Some(token) = state.ui.active_file_token() else {
        return;
    };
    let sel = state.ui.file_picker_cursor.unwrap_or(0);
    let Some(path) = state.ui.file_picker_matches.get(sel) else {
        return;
    };
    let mention = format!("@{path} ");
    state
        .ui
        .input_buffer
        .replace_range(token.start..token.query_end, &mention);
    state.ui.input_cursor = token.start + mention.len();
    state.ui.file_picker_matches.clear();
    state.ui.file_picker_cursor = None;
}

/// Clamp a raw byte offset onto the nearest preceding char boundary
/// in `s`. Callers that trust their cursor is already valid can skip
/// this; paste + multi-step transformations should use it.
pub fn clamp_cursor(s: &str, pos: usize) -> usize {
    let capped = pos.min(s.len());
    s.floor_char_boundary(capped)
}

/// Step BACK through input history (Up arrow). The first press saves
/// the user's in-progress draft and replaces the buffer with the
/// newest history entry; subsequent presses step older.
pub fn history_nav_back(state: &mut State) {
    let history = &state.session.conversation.input_history;
    if history.is_empty() {
        return;
    }
    let next_cursor = match state.ui.input_history_cursor {
        None => {
            // First Up press — snapshot the current draft.
            state.ui.history_draft = state.ui.input_buffer.clone();
            0
        },
        Some(i) => (i + 1).min(history.len() - 1),
    };
    state.ui.input_history_cursor = Some(next_cursor);
    // `input_history` is a VecDeque with newest at the back. Index
    // 0 from the end = newest, 1 = one older, etc.
    let historical = history
        .iter()
        .rev()
        .nth(next_cursor)
        .cloned()
        .unwrap_or_default();
    state.ui.input_buffer = historical;
    state.ui.input_cursor = state.ui.input_buffer.len();
}

/// Step FORWARD through input history (Down arrow). Stepping past
/// the newest entry restores the user's original draft.
pub fn history_nav_forward(state: &mut State) {
    let Some(cursor) = state.ui.input_history_cursor else {
        return;
    };
    if cursor == 0 {
        // Back to the live draft.
        state.ui.input_buffer = std::mem::take(&mut state.ui.history_draft);
        state.ui.input_cursor = state.ui.input_buffer.len();
        state.ui.input_history_cursor = None;
        return;
    }
    let new_cursor = cursor - 1;
    state.ui.input_history_cursor = Some(new_cursor);
    let historical = state
        .session
        .conversation
        .input_history
        .iter()
        .rev()
        .nth(new_cursor)
        .cloned()
        .unwrap_or_default();
    state.ui.input_buffer = historical;
    state.ui.input_cursor = state.ui.input_buffer.len();
}

/// Cycle `ReasoningLevel` through every variant, wrapping around. Used
/// by Alt+T. Order matches the `Ord` impl so the cycle walks from
/// lowest to highest and back to None.
pub fn cycle_reasoning(
    current: mermaid_model::models::ReasoningLevel,
) -> mermaid_model::models::ReasoningLevel {
    use mermaid_model::models::ReasoningLevel as R;
    match current {
        R::None => R::Minimal,
        R::Minimal => R::Low,
        R::Low => R::Medium,
        R::Medium => R::High,
        R::High => R::XHigh,
        R::XHigh => R::Max,
        R::Max => R::None,
    }
}

/// Cycle `SafetyMode` by increasing permissiveness, wrapping around. Used by
/// Shift+Tab: Plan → `ReadOnly` → Ask → Auto → `FullAccess` → Plan.
///
/// Plan is a position in the cycle like any other mode — it is the strictest
/// one (`permissiveness() == 0`), so the walk starts there. Entering it still
/// allocates a plan path and may swap the model; that side of the transition
/// lives in [`apply_safety_mode`], which every mode switch routes through.
pub fn cycle_safety(
    current: mermaid_model::safety::SafetyMode,
) -> mermaid_model::safety::SafetyMode {
    use mermaid_model::safety::SafetyMode as S;
    match current {
        S::Plan => S::ReadOnly,
        S::ReadOnly => S::Ask,
        S::Ask => S::Auto,
        S::Auto => S::FullAccess,
        S::FullAccess => S::Plan,
    }
}

/// Build and enqueue the submit for whatever is in the input buffer *right now*
/// — a slash command, or a prompt plus its staged attachments. Extracted from
/// the Enter handler so the paste-race guard can replay it verbatim once a
/// deferred clipboard read drains, re-deriving text + attachments (and thus
/// picking up a freshly-pasted image). No-op on empty/whitespace input.
pub fn submit_current_input(state: &mut State) {
    let buf = state.ui.input_buffer.trim().to_string();
    if buf.is_empty() {
        return;
    }
    if let Some(rest) = buf.strip_prefix('/') {
        // Plugin prompt commands: an enabled plugin's `/name args` expands
        // into a normal user prompt — the transcript shows the EXPANSION, so
        // recordings replay without the plugin installed. Built-ins always
        // win (the loader already refuses shadowing names; this order makes
        // it structural).
        let (name, args) = match rest.split_once(char::is_whitespace) {
            Some((n, a)) => (n.to_lowercase(), a),
            None => (rest.to_lowercase(), ""),
        };
        let builtin = crate::slash_commands::COMMAND_REGISTRY
            .iter()
            .any(|c| c.name == name || c.aliases.contains(&name.as_str()));
        if !builtin && let Some(cmd) = state.plugin_commands.iter().find(|c| c.name == name) {
            let text = cmd.expand(args);
            state.ui.input_buffer.clear();
            state.ui.input_cursor = 0;
            state.ui.palette_cursor = None;
            let attachment_ids: Vec<u64> = state.ui.attachments.iter().map(|a| a.id).collect();
            state.ui.pending_msgs.push_back(Msg::SubmitPrompt {
                text,
                attachment_ids,
            });
            return;
        }
        let slash = crate::parse_slash_command(rest);
        state.ui.input_buffer.clear();
        state.ui.input_cursor = 0;
        state.ui.palette_cursor = None;
        state.ui.pending_msgs.push_back(Msg::Slash(slash));
    } else {
        let text = std::mem::take(&mut state.ui.input_buffer);
        state.ui.input_cursor = 0;
        let attachment_ids: Vec<u64> = state.ui.attachments.iter().map(|a| a.id).collect();
        state.ui.pending_msgs.push_back(Msg::SubmitPrompt {
            text,
            attachment_ids,
        });
    }
}

/// Insert `text` at the input cursor and advance past it, resetting history-nav
/// and opening the slash palette if the buffer now starts with `/`. Shared by
/// terminal bracketed paste (`handle_paste`) and Ctrl+V text
/// (`handle_clipboard_read`) so the two agree on cursor handling — and on the
/// @-mention picker, which re-ranks here for the same reason the keystroke
/// path re-ranks: its match list is cached state, and any text mutation that
/// skips the refresh leaves it ranked for a query the user is no longer
/// looking at. Enter then completes with the stale head — paste `caf`, get
/// `@aaaa.md`.
pub fn insert_text_at_cursor(state: &mut State, cmds: &mut Vec<Cmd>, text: &str) {
    // Insert at the cursor (not the end): on the Windows console a paste arrives
    // as a mix of coalesced `Paste` chunks and stray `Char` key events, and
    // appending here while keys insert at the cursor scrambled the result
    // (uppercase letters piled at the front).
    state.ui.input_history_cursor = None;
    state.ui.history_draft.clear();
    // A paste is typing: Esc's per-token dismissal lifts the same way it
    // does for a keystroke.
    state.ui.file_picker_dismissed = false;
    let pos = clamp_cursor(&state.ui.input_buffer, state.ui.input_cursor);
    state.ui.input_buffer.insert_str(pos, text);
    state.ui.input_cursor = clamp_cursor(&state.ui.input_buffer, pos + text.len());
    if state.ui.input_buffer.starts_with('/') {
        state.ui.palette_cursor = Some(0);
    }
    refresh_file_picker(state, cmds);
}

pub fn handle_paste(state: &mut State, cmds: &mut Vec<Cmd>, paste: Paste) {
    // Terminal bracketed paste (and the Windows key-burst coalescer) is always
    // text; Ctrl+V clipboard reads — which can be images — arrive separately as
    // `Msg::ClipboardRead`.
    let Paste::Text(t) = paste;
    // A picker that filters as you type owns the keyboard, so it must own the
    // paste too. This went unnoticed because a paste normally arrives as one
    // burst: it silently typed into the composer *behind* the open pane. The
    // coalescer makes it reachable by ordinary typing — fast keystrokes get
    // batched into a paste, so a filter typed at speed lands split between the
    // pane and the hidden composer, character by character.
    if let UiMode::ModelPicker { query, cursor, .. } = &mut state.ui.mode {
        query.push_str(&t);
        // Narrowing invalidates the old row position (same rule as a keystroke).
        *cursor = 0;
        return;
    }
    insert_text_at_cursor(state, cmds, &t);
}

/// A `Cmd::ReadClipboard` (Ctrl+V) has resolved. Release the pending-read
/// counter first — even on empty/error, so a submit held by the paste-race
/// guard is never wedged — then apply the outcome, and finally fire any held
/// submit once the last in-flight read has drained.
pub fn handle_clipboard_read(state: &mut State, cmds: &mut Vec<Cmd>, read: ClipboardRead) {
    state.ui.clipboard_reads_pending = state.ui.clipboard_reads_pending.saturating_sub(1);
    match read {
        ClipboardRead::Image { bytes, format } => {
            let id = state.ids.tool_call.next();
            let number = state.ids.fresh_image();
            let temp_path = state.temp_dir.join(format!("mermaid-img-{id}.{format}"));
            // Splice the inline `[Image #N] ` token into the buffer at the
            // cursor — the token IS how the image lives in the message now, so
            // reset history-nav and advance past it.
            state.ui.input_history_cursor = None;
            state.ui.history_draft.clear();
            let token = crate::image_token::render_token(number);
            let pos = clamp_cursor(&state.ui.input_buffer, state.ui.input_cursor);
            state.ui.input_buffer.insert_str(pos, &token);
            state.ui.input_cursor = clamp_cursor(&state.ui.input_buffer, pos + token.len());
            // The token's trailing space closes any open @-token; re-rank so
            // the picker drops instead of lingering with stale matches.
            refresh_file_picker(state, cmds);
            state.ui.attachments.push(crate::state::Attachment {
                id,
                number,
                base64_data: base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    &bytes,
                ),
                temp_path: temp_path.clone(),
                size_bytes: bytes.len(),
                format: format.clone(),
            });
            cmds.push(Cmd::WriteImageToTemp {
                path: temp_path,
                bytes,
                format,
            });
            // Proactively probe whether the current model can even see this
            // image, so a no-vision warning appears now — before you send —
            // rather than after a wasted turn.
            cmds.push(Cmd::ProbeVision {
                model_id: state.session.model_id.clone(),
                warn: true,
            });
        },
        ClipboardRead::Text(t) => {
            insert_text_at_cursor(state, cmds, &t);
        },
        ClipboardRead::Empty => {
            push_system(state, cmds, "Clipboard is empty");
        },
        ClipboardRead::Error(text) => {
            push_system(state, cmds, text);
        },
    }
    // Release a submit held by the paste-race guard once the last pending read
    // drains — re-deriving text + attachments so the freshly-pasted image is
    // included.
    if state.ui.clipboard_reads_pending == 0 && state.ui.submit_after_clipboard {
        state.ui.submit_after_clipboard = false;
        submit_current_input(state);
    }
}

/// Commit one user message to the conversation: resolve its `[Image #N]`
/// tokens against the attachments it owns, drop those attachments from the
/// staging area, append the `ChatMessage`, and record input history. Shared
/// by the idle submit path and the mid-run steering drain (tool-boundary
/// delivery of queued messages) so both commit with identical semantics.
pub fn commit_user_message(state: &mut State, text: String, attachment_ids: &[u64]) {
    if text.trim().is_empty() {
        return;
    }
    // Select images by the `[Image #N]` tokens present in the submitted text, in
    // first-appearance order — the inline tokens are the source of truth. Scope
    // by `attachment_ids` (the attachments this message owns) so the busy/queued
    // path can never grab a later message's image. `images[i]` and
    // `image_numbers[i]` stay parallel so the model correlates each image block
    // with its `[Image #N]` reference.
    let numbers = crate::image_token::numbers_in_order(&text);
    let mut images: Vec<String> = Vec::new();
    let mut image_numbers: Vec<u64> = Vec::new();
    for n in &numbers {
        if let Some(a) = state
            .ui
            .attachments
            .iter()
            .find(|a| a.number == *n && attachment_ids.contains(&a.id))
        {
            images.push(a.base64_data.clone());
            image_numbers.push(*n);
        }
        // A token with no owned attachment (typed literal / mangled pill) stays
        // as plain text and simply sends no image.
    }
    // Drop every attachment this message owns — sent or orphaned — while keeping
    // any that belong to still-queued messages.
    state
        .ui
        .attachments
        .retain(|a| !attachment_ids.contains(&a.id));

    let mut user_msg = ChatMessage::user(text.clone());
    if !images.is_empty() {
        user_msg = user_msg
            .with_images(images)
            .with_image_numbers(image_numbers);
    }
    state.session.append(user_msg, state.now);
    state.session.record_input(text);
}

pub fn handle_submit_prompt(
    state: &mut State,
    cmds: &mut Vec<Cmd>,
    text: String,
    attachment_ids: &[u64],
) {
    if text.trim().is_empty() {
        return;
    }
    // If a turn is already in flight, queue this message. The
    // reducer's StreamDone arm pops the oldest queued message and
    // auto-submits it.
    if !matches!(state.turn, TurnState::Idle) {
        // Bound the queue: a user holding Enter during a long turn would
        // otherwise grow it without limit. Past the cap, drop the oldest queued
        // prompt (mirrors the `pending_msgs` drain cap).
        if state.ui.queued_messages.len() >= MAX_QUEUED_MESSAGES {
            state.ui.queued_messages.pop_front();
            tracing::warn!(
                max = MAX_QUEUED_MESSAGES,
                "reducer: queued_messages cap hit — dropped the oldest queued prompt"
            );
        }
        state
            .ui
            .queued_messages
            .push_back(crate::state::QueuedMessage {
                text,
                attachment_ids: attachment_ids.to_vec(),
            });
        return;
    }

    commit_user_message(state, text, attachment_ids);
    state.ui.input_buffer.clear();

    // The first user message derives the conversation title; every
    // subsequent message keeps it. Either way, emit SetTerminalTitle
    // only on actual change.
    emit_title_if_changed(state, cmds);

    // Instructions/memory are kept fresh by the background config watcher (#45),
    // which stamps `state.instructions`/`state.memory` via
    // `Msg::InstructionsChanged`/`MemoryChanged`. The reducer reads them here as
    // injected data — no inline I/O — so `update()` stays pure and a recorded
    // session replays without re-statting the live filesystem.
    let turn = state.ids.fresh_turn();
    // Anchor the whole user interaction here. The agentic loop will mint fresh
    // `TurnId`s for each tool follow-up, but the spinner's elapsed + token
    // counters track this run start so they don't reset at every step.
    state.runtime.run_started = Some(std::time::SystemTime::from(state.now));
    state.runtime.run_tokens = Default::default();
    state.runtime.run_line_changes = Default::default();
    // Fresh run — clear the truncation-recovery, empty-turn, and output-cap
    // continuation guards from any prior run so this intent gets a full retry
    // budget. (A run that *ended* at the continuation cap never hits the
    // in-stream reset, so without this the next run would start with a zero
    // continuation budget.)
    state.runtime.truncation_recoveries = 0;
    state.runtime.empty_continuations = 0;
    state.runtime.continue_recoveries = 0;
    state.turn = start_generating(turn, std::time::SystemTime::from(state.now));
    push_call_model(state, cmds, turn);
}

/// Handle `Msg::OpenImageAt`. Resolves the base64 payload from the committed
/// message history, writes it to a temp file, and dispatches
/// `Cmd::OpenInSystem` so the user's default image viewer opens it. F13.
///
/// Resolution prefers the stable global image number: the click map indexes
/// the DISPLAY transcript, which the continuation stitch can shift away from
/// committed history (hidden nudges, merged bubbles). The positional pair is
/// the fallback for images without a number (pre-numbering sessions — which
/// predate stitching, so their indices still align).
pub fn handle_open_image_at(
    state: &mut State,
    cmds: &mut Vec<Cmd>,
    message_index: usize,
    image_index: usize,
    image_number: Option<u64>,
) {
    let by_number = image_number.and_then(|n| {
        state.session.messages().iter().find_map(|m| {
            let pos = m.image_numbers.as_ref()?.iter().position(|&x| x == n)?;
            m.images.as_ref()?.get(pos)
        })
    });
    let b64 = match by_number {
        Some(b64) => b64,
        None => {
            let msg = match state.session.messages().get(message_index) {
                Some(m) => m,
                None => return,
            };
            let Some(images) = msg.images.as_ref() else {
                return;
            };
            let Some(b64) = images.get(image_index) else {
                return;
            };
            b64
        },
    };
    use base64::{Engine, engine::general_purpose};
    let Ok(bytes) = general_purpose::STANDARD.decode(b64) else {
        return;
    };
    let id = state.ids.tool_call.next();
    let temp_path = state.temp_dir.join(format!("mermaid-img-{id}.png"));
    cmds.push(Cmd::WriteImageToTemp {
        path: temp_path.clone(),
        bytes,
        format: "png".to_string(),
    });
    cmds.push(Cmd::OpenInSystem(temp_path));
}
