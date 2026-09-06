use crate::cmd::Cmd;
use crate::msg::KeyCode;
use crate::picker::{PickerStep, picker_step};
use crate::reducer::*;
use crate::reports::latest_user_intent;
use crate::state::{State, ToolOutcome, UiMode};
use mermaid_model::models::{ChatMessage, MessageRole};

/// Word pools for the plan-file slug suffix. Indexed by a deterministic hash
/// of (conversation id, message count) — never the wall clock or an RNG — so
/// `--replay` allocates the identical path.
pub const PLAN_SLUG_ADJECTIVES: &[&str] = &[
    "amber", "bright", "calm", "deft", "eager", "fresh", "keen", "lucid", "mellow", "neat",
    "quiet", "sharp", "steady", "swift", "tidy", "vivid",
];

pub const PLAN_SLUG_NOUNS: &[&str] = &[
    "anchor", "beacon", "compass", "current", "delta", "harbor", "inlet", "lagoon", "pearl",
    "reef", "ripple", "shoal", "spring", "strand", "tide", "wake",
];

/// FNV-1a — tiny, dependency-free, deterministic across runs (unlike
/// `DefaultHasher`, whose seed is unspecified).
#[must_use]
pub fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Allocate the plan-file path for a new plan: `.mermaid/plans/<topic>-<adj>-
/// <noun>.md` under the project root. The topic comes from the latest user
/// message (what the plan is about); the word-pair suffix keeps concurrent
/// sessions in one repo from colliding.
#[must_use]
pub fn plan_path_for(state: &State) -> std::path::PathBuf {
    let topic_src = latest_user_intent(&state.session).unwrap_or_default();
    let words: Vec<String> = topic_src
        .split_whitespace()
        .map(|w| {
            w.chars()
                .filter(|c| c.is_ascii_alphanumeric())
                .map(|c| c.to_ascii_lowercase())
                .collect::<String>()
        })
        .filter(|w| !w.is_empty())
        .take(4)
        .collect();
    let mut topic = if words.is_empty() {
        "plan".to_string()
    } else {
        words.join("-")
    };
    topic.truncate(topic.floor_char_boundary(40));
    let topic = topic.trim_end_matches('-');
    let seed = format!(
        "{}:{}",
        state.session.conversation.id,
        state.session.messages().len()
    );
    let h = fnv1a(seed.as_bytes());
    let adj = PLAN_SLUG_ADJECTIVES[(h % PLAN_SLUG_ADJECTIVES.len() as u64) as usize];
    let noun = PLAN_SLUG_NOUNS[((h >> 8) % PLAN_SLUG_NOUNS.len() as u64) as usize];
    state
        .cwd
        .join(".mermaid")
        .join("plans")
        .join(format!("{topic}-{adj}-{noun}.md"))
}

/// The plan-file path shown to the user: relative to the project root when it
/// is inside it (it always is today), absolute otherwise.
#[must_use]
pub fn plan_path_display(state: &State, path: &std::path::Path) -> String {
    path.strip_prefix(&state.cwd)
        .unwrap_or(path)
        .display()
        .to_string()
}

/// Rows in the plan-config picker. Cross-checked against the widget's
/// `plan_config_rows` by a test in `render::widgets::plan_config`.
pub const PLAN_CONFIG_ROW_COUNT: usize = 9;

/// `/plan config` picker keys: ↑/↓ navigate, Enter/←/→ cycle the highlighted
/// value (persisting the `[plan]` table on every change), Esc closes.
pub fn handle_plan_config_key(state: &mut State, cmds: &mut Vec<Cmd>, code: KeyCode) {
    let UiMode::PlanConfig { ref mut cursor } = state.ui.mode else {
        return;
    };
    match picker_step(code, cursor, PLAN_CONFIG_ROW_COUNT) {
        // Enter and Right both cycle the highlighted value forward.
        PickerStep::Confirm(row) => {
            cycle_plan_config_row(state, row, true);
            cmds.push(Cmd::PersistPlanConfig(state.settings.plan.clone()));
        },
        PickerStep::Dismiss => {
            state.ui.mode = UiMode::EditingInput;
        },
        PickerStep::Moved => {},
        PickerStep::Other => {
            let row = *cursor;
            match code {
                KeyCode::Right => {
                    cycle_plan_config_row(state, row, true);
                    cmds.push(Cmd::PersistPlanConfig(state.settings.plan.clone()));
                },
                KeyCode::Left => {
                    cycle_plan_config_row(state, row, false);
                    cmds.push(Cmd::PersistPlanConfig(state.settings.plan.clone()));
                },
                _ => {},
            }
        },
    }
}

/// Advance one picker row's value. Every change takes effect immediately —
/// the permission profile rides each tool dispatch, and the model/reasoning
/// overrides are read at the next plan-mode entry.
pub fn cycle_plan_config_row(state: &mut State, row: usize, forward: bool) {
    use crate::{PlanPermLevel as L, PlanPermissions, PlanPostApprove};
    fn cycle<T: Copy + PartialEq>(order: &[T], current: T, forward: bool) -> T {
        let idx = order.iter().position(|v| *v == current).unwrap_or(0);
        let len = order.len();
        let next = if forward {
            (idx + 1) % len
        } else {
            (idx + len - 1) % len
        };
        order[next]
    }
    const LEVELS: [L; 4] = [L::Allow, L::Auto, L::Ask, L::Deny];
    let session_model = state.session.model_id.clone();
    let plan = &mut state.settings.plan;
    match row {
        0 => {
            // Preset cycle; a custom profile snaps to the nearest preset
            // boundary (default going forward, open going back).
            let presets = [
                PlanPermissions::default(),
                PlanPermissions::strict(),
                PlanPermissions::open(),
            ];
            let idx = presets.iter().position(|p| *p == plan.permissions);
            plan.permissions = match (idx, forward) {
                (Some(i), true) => presets[(i + 1) % 3],
                (Some(i), false) => presets[(i + 2) % 3],
                (None, true) => presets[0],
                (None, false) => presets[2],
            };
        },
        1 => plan.permissions.builds = cycle(&LEVELS, plan.permissions.builds, forward),
        2 => plan.permissions.web = cycle(&LEVELS, plan.permissions.web, forward),
        3 => plan.permissions.memory = cycle(&LEVELS, plan.permissions.memory, forward),
        // Task tools are ungated (no approval path): allow/deny only.
        4 => {
            plan.permissions.tasks = if plan.permissions.tasks == L::Allow {
                L::Deny
            } else {
                L::Allow
            };
        },
        // Unset <-> pin to the current session model. Other models are set
        // by editing `[plan] model` in config.toml (the handoff picker is
        // the searchable surface).
        5 => {
            plan.model = match plan.model {
                Some(_) => None,
                None => Some(session_model),
            };
        },
        6 => {
            use mermaid_model::models::ReasoningLevel as R;
            const REASONING: [Option<R>; 8] = [
                None,
                Some(R::None),
                Some(R::Minimal),
                Some(R::Low),
                Some(R::Medium),
                Some(R::High),
                Some(R::XHigh),
                Some(R::Max),
            ];
            plan.reasoning = cycle(&REASONING, plan.reasoning, forward);
        },
        7 => plan.auto_approve = !plan.auto_approve,
        8 => {
            const POST: [Option<PlanPostApprove>; 3] = [
                None,
                Some(PlanPostApprove::Start),
                Some(PlanPostApprove::Wait),
            ];
            plan.post_approve = cycle(&POST, plan.post_approve, forward);
        },
        _ => {},
    }
}

/// Switch the session safety mode, running whatever the transition needs.
///
/// The ONE entry point for every interactive mode change (Shift+Tab,
/// `/safety <mode>`), because `Plan` is a mode with side effects on both
/// edges: entering allocates the plan file and applies the `[plan]`
/// model/reasoning overrides, leaving tears them back down. Routing every
/// switch through here is what lets plan sit in the flat Shift+Tab cycle
/// without a separate "am I planning?" flag to contradict it.
pub fn apply_safety_mode(
    state: &mut State,
    cmds: &mut Vec<Cmd>,
    next: mermaid_model::safety::SafetyMode,
) {
    let previous = state.session.safety_mode;
    if previous == next {
        return;
    }
    if next.is_planning() {
        // Entry does the full flip (mode included) and persists.
        enter_plan_mode_state(state, cmds);
        return;
    }
    if let Some(plan) = state.session.plan.take() {
        // Leaving plan for a real permission level: undo the `[plan]`
        // overrides, drop the standing reminder, then land on the mode the
        // user actually picked (NOT a remembered restore target — there
        // isn't one any more).
        restore_plan_overrides(state, &plan);
        retract_plan_reminder(state);
    }
    state.session.safety_mode = next;
    // Leaving read_only past a stale denial nudges the model to re-attempt
    // (hidden from the transcript); `build_chat_request` additionally rewrites
    // the stale denials themselves.
    note_safety_mode_change(state, cmds, previous, next);
    // Persist now so `--resume`/`--continue` restore this mode even if the user
    // changes it and quits without sending another message.
    cmds.push(state.session.save_conversation_cmd());
}

/// Where a session lands when it leaves plan mode without naming a
/// destination — `/plan off`, plan approval, a handoff. The configured
/// `[safety] mode` (the level this session would be in had it never planned),
/// clamped away from `plan` itself so "leave" can never mean "stay".
///
/// There is deliberately no remembered per-session restore target: plan is a
/// safety mode like the others, and a mode does not carry the mode before it.
#[must_use]
pub fn mode_after_plan(state: &State) -> mermaid_model::safety::SafetyMode {
    let configured = state.settings.safety.mode;
    if configured.is_planning() {
        mermaid_model::safety::SafetyMode::Ask
    } else {
        configured
    }
}

/// The pure state flip of entering plan mode: allocate the plan file path,
/// set `session.plan`, retract stale nudges, persist. Shared by the
/// interactive entry (Shift+Tab / `/plan` / `/safety plan`) and the
/// `enter_plan_mode` tool — the tool path runs mid-batch, where appending a
/// system message would interleave between an assistant `tool_use` and its
/// tool results, so THIS helper never touches the message log. Returns the
/// allocated path; `None` when already planning or a subagent.
pub fn enter_plan_mode_state(state: &mut State, cmds: &mut Vec<Cmd>) -> Option<std::path::PathBuf> {
    // Children explore, they don't plan (and have no user to approve).
    if state.session.is_subagent || state.session.plan.is_some() {
        return None;
    }
    let plan_path = plan_path_for(state);
    // The `[plan]` model/reasoning overrides: swap on entry, stash what to
    // restore. Per-request routing makes this safe — the very next dispatch
    // resolves the new model lazily.
    let mut prev_model_id = None;
    let mut prev_reasoning = None;
    if let Some(plan_model) = state.settings.plan.model.clone()
        && plan_model != state.session.model_id
    {
        prev_model_id = Some(std::mem::replace(
            &mut state.session.model_id,
            plan_model.clone(),
        ));
        state.runtime.set_model(&plan_model);
    }
    if let Some(plan_reasoning) = state.settings.plan.reasoning
        && plan_reasoning != state.session.reasoning
    {
        prev_reasoning = Some(std::mem::replace(
            &mut state.session.reasoning,
            plan_reasoning,
        ));
    }
    // The mode BECOMES plan. `session.plan` carries only the plan's data, so
    // there is no second value that can disagree with the floor.
    state.session.safety_mode = mermaid_model::safety::SafetyMode::Plan;
    state.session.plan = Some(crate::state::PlanState {
        plan_path: plan_path.clone(),
        prev_model_id,
        prev_reasoning,
    });
    // Retract any pending safety-mode nudge: "re-attempt gated actions" would
    // steer the model wrong now that the read-only floor applies. (The old
    // plan-exit nudge is gone — the context-delta injector diffs at dispatch,
    // so an off→on flip between dispatches collapses to no message at all.)
    state.session.conversation.messages_mut().retain(|m| {
        m.kind != mermaid_model::models::ChatMessageKind::RecoveryNudge
            || !m.content.starts_with(SAFETY_NUDGE_PREFIX)
    });
    // A fresh plan starts with a disarmed doom-loop breaker.
    state.runtime.plan_thrash_armed = false;
    state.runtime.plan_calls_since_denial = 0;
    cmds.push(state.session.save_conversation_cmd());
    Some(plan_path)
}

/// Interactive plan-mode entry (`/plan`, `/safety plan`, Shift+Tab): the state
/// flip only — the status band announces the mode (`safety: plan`, like every
/// other mode), so no transcript row is added HERE. The model learns the mode
/// from the context-delta marker the injector appends at the next dispatch
/// (`advertise_context_changes`), the per-dispatch tail reminder, and the
/// system-prompt appendix.
pub fn enter_plan_mode(state: &mut State, cmds: &mut Vec<Cmd>) {
    if let Some(plan) = &state.session.plan {
        let path = plan_path_display(state, &plan.plan_path.clone());
        push_system(
            state,
            cmds,
            format!("Already in plan mode — plan file: {path} (Shift+Tab or /plan off leaves)"),
        );
        return;
    }
    enter_plan_mode_state(state, cmds);
}

/// Undo the `[plan]` model/reasoning overrides stashed at entry, and leave the
/// `Plan` mode for [`mode_after_plan`] if the caller has not already picked a
/// destination. Callers that DO pick one (`apply_safety_mode`) overwrite
/// `safety_mode` right after this returns.
pub fn restore_plan_overrides(state: &mut State, plan: &crate::state::PlanState) {
    if let Some(prev) = &plan.prev_model_id {
        state.session.model_id = prev.clone();
        state.runtime.set_model(prev);
    }
    if let Some(prev) = plan.prev_reasoning {
        state.session.reasoning = prev;
    }
    if state.session.safety_mode.is_planning() {
        state.session.safety_mode = mode_after_plan(state);
    }
}

/// Leave plan mode without an approval flow (`/plan off`). Lands on
/// [`mode_after_plan`] — the level the session would have been in had it never
/// planned. Shift+Tab out of plan goes through [`apply_safety_mode`] instead,
/// which lands on the next mode in the cycle.
pub fn exit_plan_mode(state: &mut State, cmds: &mut Vec<Cmd>) {
    let Some(plan) = state.session.plan.take() else {
        push_system(
            state,
            cmds,
            "Not in plan mode (/plan or Shift+Tab enters it)",
        );
        return;
    };
    restore_plan_overrides(state, &plan);
    retract_plan_reminder(state);
    // No transcript row: the status band reverting to the plain safety mode
    // is the human announcement; the model's comes from the context-delta
    // marker at the next dispatch.
    cmds.push(state.session.save_conversation_cmd());
}

/// Retract a standing plan tail reminder and disarm the doom-loop breaker —
/// every `session.plan -> None` transition must call this, or an exit mid-run
/// leaves "plan mode is active" riding the next boundary dispatch (and a
/// stale thrash counter waiting for the next plan).
pub fn retract_plan_reminder(state: &mut State) {
    state.session.conversation.messages_mut().retain(|m| {
        m.kind != mermaid_model::models::ChatMessageKind::RecoveryNudge
            || !m.content.starts_with(PLAN_REMINDER_PREFIX)
    });
    state.runtime.plan_thrash_armed = false;
    state.runtime.plan_calls_since_denial = 0;
}

/// The `content` infix that marks a persisted **plan-mode** policy denial.
/// Sibling of [`readonly_denial_signature`], keyed on
/// [`mermaid_model::safety::PLAN_DENIAL_MARKER`].
#[must_use]
pub fn plan_denial_signature() -> String {
    format!(
        "blocked by policy: {}",
        mermaid_model::safety::PLAN_DENIAL_MARKER
    )
}

/// True if the conversation still carries a plan-mode policy denial.
#[must_use]
pub fn history_has_plan_denial(messages: &[ChatMessage]) -> bool {
    let signature = plan_denial_signature();
    messages
        .iter()
        .any(|m| m.role == MessageRole::Tool && m.content.contains(&signature))
}

/// Plan-mode sibling of [`neutralize_superseded_policy_denials`]: once plan
/// mode ends, its denials stop describing the live policy — rewrite them to a
/// past-tense note so the wire history stops contradicting the current mode.
/// No-op while planning (the denials still apply).
pub fn neutralize_superseded_plan_denials(messages: &mut [ChatMessage], plan_active: bool) {
    if plan_active {
        return;
    }
    let signature = plan_denial_signature();
    for msg in messages.iter_mut() {
        if msg.role != MessageRole::Tool || !msg.content.contains(&signature) {
            continue;
        }
        let summary = msg
            .content
            .split_once(" blocked by policy: ")
            .map(|(head, _)| head.trim_end())
            .filter(|head| !head.is_empty())
            .unwrap_or("The action");
        msg.content = format!(
            "{summary} was blocked earlier while a plan was being drafted. Plan \
             mode is now off — that restriction no longer applies; re-run it if \
             the plan calls for it."
        );
    }
}

/// Seed the checklist from the plan's Tasks section. Re-plan reconcile:
/// completed items survive (their subjects aren't re-seeded), everything
/// still open is replaced by the new plan's steps. `Stamp::default()` keeps
/// it replay-deterministic; the wholesale `SyncTaskStore` mirrors the
/// fork/clear reset path (the broker's `seed` doesn't publish — the reducer
/// already holds the truth).
pub fn seed_plan_tasks(state: &mut State, cmds: &mut Vec<Cmd>, body: &str) {
    let specs = crate::plan::parse_plan_tasks(body);
    if specs.is_empty() {
        return;
    }
    use crate::checklist::{ChecklistEdit, ChecklistOrigin, ChecklistStatus, Stamp};
    let mut store = state.session.conversation.tasks.clone();
    let completed: std::collections::HashSet<String> = store
        .visible()
        .filter(|t| t.status == ChecklistStatus::Completed)
        .map(|t| t.subject.trim().to_ascii_lowercase())
        .collect();
    let stale: Vec<ChecklistEdit> = store
        .visible()
        .filter(|t| t.status != ChecklistStatus::Completed)
        .map(|t| ChecklistEdit {
            id: t.id,
            status: Some(ChecklistStatus::Deleted),
            subject: None,
            active_form: None,
            description: None,
        })
        .collect();
    if !stale.is_empty() {
        store.apply(&stale, Stamp::default());
    }
    let fresh: Vec<_> = specs
        .into_iter()
        .filter(|s| !completed.contains(&s.subject.trim().to_ascii_lowercase()))
        .collect();
    if !fresh.is_empty() {
        store.create(fresh, ChecklistOrigin::Model, Stamp::default());
    }
    state.session.conversation.tasks = store.clone();
    cmds.push(Cmd::SyncTaskStore(store));
}

/// The user approved the plan into a NEW conversation (clear-context
/// execute, or an explicit handoff): persist the exploration transcript,
/// mint the next conversation (fork carries the transcript + checklist;
/// fresh starts from the plan alone), optionally switch models, seed the
/// checklist, and drive the kickoff turn.
pub fn handoff_plan_mode(
    state: &mut State,
    cmds: &mut Vec<Cmd>,
    body: &str,
    fresh: bool,
    fork: bool,
    model: Option<String>,
) {
    let Some(plan) = state.session.plan.take() else {
        return;
    };
    restore_plan_overrides(state, &plan);
    // The exploration conversation is history now — save it under its own id
    // BEFORE swapping (the fork_conversation_at ordering).
    cmds.push(state.session.save_conversation_cmd());
    let original = &state.session.conversation;
    let mut next = crate::ConversationHistory::new(
        original.project_path.clone(),
        original.model_name.clone(),
        state.now,
    );
    // Millisecond-derived ids: bump deterministically on collision (the
    // rewind-fork precedent) so the two sessions never share a save file.
    if next.id == original.id {
        next = crate::ConversationHistory::new(
            original.project_path.clone(),
            original.model_name.clone(),
            state.now + chrono::Duration::milliseconds(1),
        );
    }
    if fork {
        next.title = original.title.clone();
        next.set_messages(original.messages().to_vec());
        next.input_history = original.input_history.clone();
        next.git_branch = original.git_branch.clone();
        next.tasks = original.tasks.clone();
        next.forked_from = Some(original.id.clone());
        // The forked transcript carries the plan-ON marker, so the injector
        // baseline rides along: its first dispatch diffs "advertised planning"
        // against live plan-off and announces the exit IN THE NEW conversation
        // — a timeline transition-point injection could never produce (the
        // transition ran in the old one). Fresh handoffs keep `None`: a
        // context that never saw plan mode has nothing to reconcile.
        next.advertised_context = original.advertised_context.clone();
    }
    state.session.replace_conversation(next);
    // A standing plan reminder rode in with the forked messages (it is only
    // swept at turn-end); plan mode is over, retract it.
    retract_plan_reminder(state);
    if let Some(model) = model {
        state.session.model_id = model.clone();
        state.runtime.set_model(&model);
    }
    seed_plan_tasks(state, cmds, body);
    // Fresh contexts get the handoff preamble + the plan as their opening
    // user message (the plan IS the brief); a fork already carries the plan
    // in its transcript, so a bare kickoff suffices.
    let kickoff = if fresh {
        format!(
            "{}

{}",
            crate::prompts::PLAN_HANDOFF_PREAMBLE,
            body
        )
    } else {
        "Implement the plan.".to_string()
    };
    state
        .ui
        .queued_messages
        .push_back(crate::state::QueuedMessage {
            text: kickoff,
            attachment_ids: vec![],
        });
    drain_next_queued_message(state);
    cmds.push(state.session.save_conversation_cmd());
}

/// Plan-tool post-processing at the tool boundary (`handle_tool_finished`):
/// flip the plan state so everything the follow-up model call derives —
/// system prompt, tool advertisement, dispatch flooring — sees the new mode.
/// Message-log appends are deliberately avoided here (a system message now
/// would interleave between the assistant `tool_use` and its tool results);
/// the transcript record is the rendered `ToolMetadata::Plan` block, and the
/// per-request denial neutralizer handles history hygiene.
/// Returns `true` when the outcome triggered a conversation handoff (fresh
/// or fork) — the caller must then abandon the executing turn instead of
/// appending its tool results into the NEW conversation.
pub fn plan_tool_transition(
    state: &mut State,
    cmds: &mut Vec<Cmd>,
    call_id: mermaid_model::ids::ToolCallId,
    outcome: &ToolOutcome,
) -> bool {
    use crate::plan::{ENTER_PLAN_MODE_TOOL, EXIT_PLAN_MODE_TOOL};
    let tool_name = match &state.turn {
        TurnState::ExecutingTools { calls, .. } => calls
            .iter()
            .find(|c| c.call_id == call_id)
            .map(|c| c.source.function.name.clone()),
        _ => None,
    };
    match tool_name.as_deref() {
        Some(name)
            if name == ENTER_PLAN_MODE_TOOL && outcome.status == crate::ToolStatus::Success =>
        {
            enter_plan_mode_state(state, cmds);
        },
        Some(name) if name == EXIT_PLAN_MODE_TOOL => {
            if let crate::ToolMetadata::Plan {
                body,
                start,
                fresh,
                fork,
                model,
                ..
            } = &outcome.metadata.detail
            {
                if *fresh || *fork || model.is_some() {
                    let (body, fresh, fork, model) = (body.clone(), *fresh, *fork, model.clone());
                    handoff_plan_mode(state, cmds, &body, fresh, fork, model);
                    return true;
                }
                let (body, start) = (body.clone(), *start);
                finish_plan_mode(state, cmds, &body, start);
            }
        },
        _ => {},
    }
    false
}

/// The user approved the plan (`exit_plan_mode` returned `ToolMetadata::
/// Plan`): leave plan mode, seed the checklist from the plan's Tasks section,
/// and optionally queue the implementation kickoff.
pub fn finish_plan_mode(state: &mut State, cmds: &mut Vec<Cmd>, body: &str, start: bool) {
    let Some(plan) = state.session.plan.take() else {
        // Stale or duplicate approval — nothing to transition.
        return;
    };
    restore_plan_overrides(state, &plan);
    // Retract the standing plan tail reminder; the per-request neutralizer
    // rewrites the denials themselves, and the context-delta marker at the
    // next dispatch tells the model plan mode is off.
    retract_plan_reminder(state);
    seed_plan_tasks(state, cmds, body);
    cmds.push(state.session.save_conversation_cmd());
    if start {
        // Auto-submit implementation through the queued-message path (the
        // background-report precedent): the tool-boundary drain in
        // `handle_tool_finished` commits it before the follow-up model call,
        // so approval flows straight into implementation in one turn.
        state
            .ui
            .queued_messages
            .push_back(crate::state::QueuedMessage {
                text: "Implement the plan.".to_string(),
                attachment_ids: vec![],
            });
    }
}
