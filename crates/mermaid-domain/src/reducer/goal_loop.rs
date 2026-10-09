//! `/goal` in the reducer: set, show and clear a goal, check it when a run
//! would end, and start the next goal turn while the check says "not yet".
//!
//! The loop lives in the run's own lifecycle. A run that would complete with
//! a goal set enters `TurnState::EvaluatingGoal` instead of ending; the
//! check's verdict either starts another `Generating` turn in the same run or
//! ends the run as usual. Esc, an error, a message from the user, the turn
//! cap and the stall guard all hand control back with the goal still set;
//! only "met", "impossible" and `/goal clear` remove it.

use crate::build_chat_request;
use crate::cmd::Cmd;
use crate::goal::{self, GoalArg, GoalOutcome, GoalProgress, GoalVerdict};
use crate::msg::Msg;
use crate::reducer::*;
use crate::state::{State, TurnState};
use mermaid_model::ids::TurnId;
use mermaid_model::models::ChatMessageKind;

/// Session spend so far (input plus output), the baseline a goal's own spend
/// is measured from.
fn session_tokens(state: &State) -> usize {
    let totals = state.session.cumulative_token_usage;
    totals
        .prompt_tokens
        .saturating_add(totals.completion_tokens)
}

fn elapsed_secs(state: &State, since: Option<std::time::SystemTime>) -> u64 {
    since
        .and_then(|t| {
            std::time::SystemTime::from(state.now)
                .duration_since(t)
                .ok()
        })
        .map_or(0, |d| d.as_secs())
}

fn plural(n: u32, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

/// `/goal [condition|clear]`.
pub fn handle_slash_goal(state: &mut State, cmds: &mut Vec<Cmd>, arg: Option<&str>) {
    match goal::parse_arg(arg) {
        GoalArg::Status => {
            let text = goal_status(state);
            push_system(state, cmds, text);
        },
        GoalArg::Clear => clear_goal(state, cmds),
        GoalArg::Set(condition) => set_goal(state, cmds, condition.to_string()),
    }
}

fn set_goal(state: &mut State, cmds: &mut Vec<Cmd>, condition: String) {
    let len = condition.chars().count();
    if len > goal::MAX_CONDITION_CHARS {
        push_system(
            state,
            cmds,
            format!(
                "The goal has {len} characters. The limit is {}.",
                goal::MAX_CONDITION_CHARS
            ),
        );
        return;
    }
    let now = std::time::SystemTime::from(state.now);
    state.runtime.goal = state.runtime.goal.start(now, session_tokens(state));
    if let Some(old) = state.session.conversation.goal.replace(condition.clone()) {
        push_system(
            state,
            cmds,
            format!("Goal replaced. The old goal was: {old}"),
        );
    }
    cmds.push(state.session.save_conversation_cmd());
    // The condition is the first turn's directive. Queued behind a turn that
    // is still running, like any other message.
    state.ui.pending_msgs.push_back(Msg::SubmitPrompt {
        text: goal::directive(&condition),
        attachment_ids: Vec::new(),
    });
}

fn clear_goal(state: &mut State, cmds: &mut Vec<Cmd>) {
    let Some(condition) = state.session.conversation.goal.take() else {
        push_system(state, cmds, "No goal set.");
        return;
    };
    state.runtime.goal = GoalProgress {
        last_outcome: state.runtime.goal.last_outcome.take(),
        ..GoalProgress::default()
    };
    push_system(state, cmds, format!("Goal cleared: {condition}"));
}

/// The `/goal` status text.
fn goal_status(state: &State) -> String {
    let progress = &state.runtime.goal;
    let Some(condition) = &state.session.conversation.goal else {
        return match &progress.last_outcome {
            Some(outcome) => format!(
                "No goal set. The last goal {}: {}\n{} · {} · {} tokens",
                if outcome.met {
                    "was met"
                } else {
                    "stopped as impossible"
                },
                outcome.condition,
                goal::format_elapsed(outcome.elapsed_secs),
                plural(outcome.checks, "check"),
                crate::compaction::format_compact_count(outcome.tokens),
            ),
            None => "No goal set. Usage: /goal <condition>. Mermaid then keeps working \
                     until a check finds the condition met."
                .to_string(),
        };
    };
    let mut lines = vec![format!("Goal: {condition}")];
    match progress.started {
        Some(started) => lines.push(format!(
            "Running for {} · {} · {} tokens",
            goal::format_elapsed(elapsed_secs(state, Some(started))),
            plural(progress.checks, "check"),
            crate::compaction::format_compact_count(
                session_tokens(state).saturating_sub(progress.tokens_at_start)
            ),
        )),
        None => lines.push(
            "Restored with this session. The next check runs when your next message's run ends."
                .to_string(),
        ),
    }
    if let Some(reason) = &progress.last_reason {
        lines.push(format!("Last check: {reason}"));
    }
    lines.push("/goal clear removes it.".to_string());
    lines.join("\n")
}

/// A message from the user restarts the turn cap and the stall guard.
pub fn note_user_prompt(state: &mut State) {
    let progress = &mut state.runtime.goal;
    progress.turns_since_prompt = 0;
    progress.idle_turns = 0;
    progress.used_tools = false;
}

/// Called where a run would complete. Returns `true` when a goal check took
/// over the run, so the caller must not end it.
pub fn begin_goal_check(state: &mut State, cmds: &mut Vec<Cmd>) -> bool {
    let Some(condition) = state.session.conversation.goal.clone() else {
        return false;
    };
    // The user's queued message runs first; the goal is checked after it.
    if !state.ui.queued_messages.is_empty() {
        return false;
    }
    // A background agent's report arrives as the next run; check after it.
    let agents = state.runtime.background_agents.len();
    if agents > 0 {
        if !state.runtime.goal.waiting_on_agents {
            state.runtime.goal.waiting_on_agents = true;
            push_system(
                state,
                cmds,
                format!(
                    "The goal check waits for {} to finish.",
                    plural(agents as u32, "background agent")
                ),
            );
        }
        return false;
    }
    let now = std::time::SystemTime::from(state.now);
    if state.runtime.goal.started.is_none() {
        state.runtime.goal.started = Some(now);
        state.runtime.goal.tokens_at_start = session_tokens(state);
    }
    state.runtime.goal.waiting_on_agents = false;
    let model = state
        .settings
        .goal
        .model
        .clone()
        .unwrap_or_else(|| state.session.model_id.clone());
    let request = goal::check_request(
        &build_chat_request(state),
        &model,
        &condition,
        state.session.messages(),
        state.runtime.goal.checks + 1,
    );
    let turn = state.ids.fresh_turn();
    state.turn = TurnState::EvaluatingGoal {
        id: turn,
        started: now,
    };
    cmds.push(Cmd::EvaluateGoal { turn, request });
    true
}

/// `Msg::GoalEvaluated`.
pub fn handle_goal_evaluated(
    state: &mut State,
    cmds: &mut Vec<Cmd>,
    turn: TurnId,
    reply: Result<goal::GoalReply, String>,
) {
    // A late reply for a check the user cancelled is the cancel's echo.
    if !matches!(state.turn, TurnState::EvaluatingGoal { id, .. } if id == turn) {
        return;
    }
    state.turn = TurnState::Idle;
    if let Ok(reply) = &reply
        && let Some(usage) = &reply.usage
    {
        // The check runs on `[goal] model` when set, else the session's.
        let model = state
            .settings
            .goal
            .model
            .clone()
            .unwrap_or_else(|| state.session.model_id.clone());
        fold_token_usage(
            &mut state.session,
            &mut state.runtime,
            usage,
            UsageFold::GoalCheck,
            UsageAttribution::Model(&model),
        );
    }
    // `/goal clear` while the check ran: nothing left to judge.
    let Some(condition) = state.session.conversation.goal.clone() else {
        end_run(state, cmds);
        return;
    };
    let verdict = match reply {
        Err(error) => {
            pause(state, cmds, &format!("the goal check failed ({error})"));
            return;
        },
        Ok(reply) => match goal::parse_reply(&reply) {
            Some(verdict) => verdict,
            None => {
                pause(state, cmds, "the goal check gave no verdict");
                return;
            },
        },
    };
    state.runtime.goal.checks += 1;
    match verdict {
        GoalVerdict::Met(reason) => {
            finish_goal(state, cmds, condition, true, &reason);
        },
        GoalVerdict::Impossible(reason) => {
            finish_goal(state, cmds, condition, false, &reason);
        },
        GoalVerdict::NotMet(reason) => continue_goal(state, cmds, &condition, reason),
    }
}

fn continue_goal(state: &mut State, cmds: &mut Vec<Cmd>, condition: &str, reason: String) {
    let progress = &mut state.runtime.goal;
    progress.last_reason = Some(reason.clone());
    progress.idle_turns = if progress.used_tools {
        0
    } else {
        progress.idle_turns + 1
    };
    progress.used_tools = false;
    let idle_turns = progress.idle_turns;
    let turns = progress.turns_since_prompt;
    // The user typed while the check ran: their message goes next.
    if !state.ui.queued_messages.is_empty() {
        push_system(state, cmds, format!("Goal not met yet: {reason}"));
        end_run(state, cmds);
        return;
    }
    if idle_turns >= goal::STALL_TURNS {
        push_system(state, cmds, format!("Goal not met yet: {reason}"));
        pause(
            state,
            cmds,
            &format!(
                "{} in a row ended without a tool call",
                plural(goal::STALL_TURNS, "goal turn")
            ),
        );
        return;
    }
    let max_turns = state.settings.goal.max_turns;
    if max_turns > 0 && turns >= max_turns {
        push_system(state, cmds, format!("Goal not met yet: {reason}"));
        pause(
            state,
            cmds,
            &format!(
                "it ran {} without a message from you ([goal] max_turns)",
                plural(max_turns, "goal turn")
            ),
        );
        return;
    }
    state.runtime.goal.turns_since_prompt += 1;
    push_system_kind(
        state,
        cmds,
        goal::continue_note(condition, &reason),
        ChatMessageKind::GoalCheck,
    );
    // A check that ran after a background agent's report starts from an
    // ended run; give the next turn a run of its own.
    let now = std::time::SystemTime::from(state.now);
    if state.runtime.run_started.is_none() {
        state.runtime.run_started = Some(now);
        state.runtime.run_tokens = Default::default();
        state.runtime.run_line_changes = Default::default();
    }
    let next = state.ids.fresh_turn();
    state.turn = crate::transition::start_generating(next, now);
    push_call_model(state, cmds, next);
}

fn finish_goal(state: &mut State, cmds: &mut Vec<Cmd>, condition: String, met: bool, reason: &str) {
    let progress = &state.runtime.goal;
    let outcome = GoalOutcome {
        condition,
        met,
        elapsed_secs: elapsed_secs(state, progress.started),
        checks: progress.checks,
        tokens: session_tokens(state).saturating_sub(progress.tokens_at_start),
        reason: reason.to_string(),
    };
    state.session.conversation.goal = None;
    state.runtime.goal = GoalProgress {
        last_outcome: Some(outcome),
        ..GoalProgress::default()
    };
    let text = if met {
        format!("Goal met: {reason}")
    } else {
        format!("Goal stopped. The check found it impossible: {reason}")
    };
    push_system(state, cmds, text);
    end_run(state, cmds);
}

/// Hand control back with the goal still set.
fn pause(state: &mut State, cmds: &mut Vec<Cmd>, why: &str) {
    push_system(
        state,
        cmds,
        format!("Goal paused: {why}. Send a message to continue, or /goal clear to remove it."),
    );
    end_run(state, cmds);
}

fn end_run(state: &mut State, cmds: &mut Vec<Cmd>) {
    finish_run(state, cmds, RunEnd::Completed);
    drain_next_queued_message(state);
}

/// The note for a goal run that an error or Esc stopped.
pub fn note_goal_interrupted(state: &mut State, cmds: &mut Vec<Cmd>) {
    if state.session.conversation.goal.is_some() {
        push_system(
            state,
            cmds,
            "Goal paused. Send a message to continue, or /goal clear to remove it.",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Config, SlashCmd};
    use mermaid_model::models::MessageRole;

    fn fresh_state() -> State {
        State::new(
            Config::default(),
            std::path::PathBuf::from("/tmp/project"),
            "ollama/test".to_string(),
            chrono::Local::now(),
            std::path::PathBuf::from("/tmp"),
        )
    }

    fn set(state: State, condition: &str) -> (State, Vec<Cmd>) {
        crate::update(
            state,
            Msg::Slash(SlashCmd::Goal(Some(condition.to_string()))),
        )
    }

    /// The model replies with `text` and ends its turn.
    fn reply(state: State, text: &str) -> (State, Vec<Cmd>) {
        let turn = state.turn.id().expect("a turn is running");
        let (state, mut cmds) = crate::update(
            state,
            Msg::StreamText {
                turn,
                chunk: text.to_string(),
            },
        );
        let (state, more) = crate::update(
            state,
            Msg::StreamDone {
                turn,
                usage: None,
                provider_continuation: None,
                stop_reason: None,
            },
        );
        cmds.extend(more);
        (state, cmds)
    }

    fn verdict(state: State, text: &str) -> (State, Vec<Cmd>) {
        let turn = state.turn.id().expect("a check is running");
        crate::update(
            state,
            Msg::GoalEvaluated {
                turn,
                reply: Ok(goal::GoalReply {
                    text: text.to_string(),
                    reasoning: None,
                    usage: None,
                }),
            },
        )
    }

    fn notes(state: &State) -> Vec<String> {
        state
            .session
            .messages()
            .iter()
            .filter(|m| m.role == MessageRole::System)
            .map(|m| m.content.clone())
            .collect()
    }

    fn has_cmd(cmds: &[Cmd], pred: impl Fn(&Cmd) -> bool) -> bool {
        cmds.iter().any(pred)
    }

    #[test]
    fn setting_a_goal_starts_a_turn_with_the_condition() {
        let (state, cmds) = set(fresh_state(), "all tests pass");
        assert_eq!(
            state.session.conversation.goal.as_deref(),
            Some("all tests pass")
        );
        assert!(matches!(state.turn, TurnState::Generating { .. }));
        assert!(has_cmd(&cmds, |c| matches!(c, Cmd::CallModel { .. })));
        let first = &state.session.messages()[0];
        assert_eq!(first.role, MessageRole::User);
        assert_eq!(first.content, "Goal: all tests pass");
    }

    #[test]
    fn a_run_that_would_end_is_checked_first() {
        let (state, _) = set(fresh_state(), "all tests pass");
        let (state, cmds) = reply(state, "I ran the tests.");
        assert!(matches!(state.turn, TurnState::EvaluatingGoal { .. }));
        let Some(Cmd::EvaluateGoal { request, .. }) =
            cmds.iter().find(|c| matches!(c, Cmd::EvaluateGoal { .. }))
        else {
            panic!("no check dispatched: {cmds:?}");
        };
        assert_eq!(request.model_id, "ollama/test");
        assert!(request.tools.is_empty());
        assert!(request.messages[0].content.contains("I ran the tests."));
        // The run is still going: no summary yet.
        assert!(
            !state
                .session
                .messages()
                .iter()
                .any(|m| m.kind == ChatMessageKind::RunSummary)
        );
    }

    #[test]
    fn the_configured_model_runs_the_check() {
        let mut state = fresh_state();
        state.settings.goal.model = Some("anthropic/small".to_string());
        let (state, _) = set(state, "done");
        let (_, cmds) = reply(state, "ok");
        assert!(has_cmd(&cmds, |c| matches!(
            c,
            Cmd::EvaluateGoal { request, .. } if request.model_id == "anthropic/small"
        )));
    }

    #[test]
    fn not_met_starts_the_next_turn_with_the_reason_and_the_goal() {
        let (state, _) = set(fresh_state(), "all tests pass");
        let (state, _) = reply(state, "I ran the tests.");
        let (state, cmds) = verdict(state, "NOT_MET: two tests still fail");
        assert!(matches!(state.turn, TurnState::Generating { .. }));
        let Some(Cmd::CallModel { request, .. }) =
            cmds.iter().find(|c| matches!(c, Cmd::CallModel { .. }))
        else {
            panic!("no next turn: {cmds:?}");
        };
        let note = request
            .messages
            .iter()
            .find(|m| m.kind == ChatMessageKind::GoalCheck)
            .expect("the next request carries the goal check");
        assert_eq!(
            note.content,
            "Goal not met yet: two tests still fail\nGoal: all tests pass"
        );
        assert_eq!(state.runtime.goal.checks, 1);
        assert_eq!(
            state.session.conversation.goal.as_deref(),
            Some("all tests pass")
        );
    }

    #[test]
    fn met_clears_the_goal_and_ends_the_run() {
        let (state, _) = set(fresh_state(), "all tests pass");
        let (state, _) = reply(state, "Done, 12 passed.");
        let (state, _) = verdict(state, "MET: 12 passed, 0 failed");
        assert!(matches!(state.turn, TurnState::Idle));
        assert_eq!(state.session.conversation.goal, None);
        assert!(
            notes(&state)
                .iter()
                .any(|n| n == "Goal met: 12 passed, 0 failed")
        );
        assert!(
            state
                .session
                .messages()
                .iter()
                .any(|m| m.kind == ChatMessageKind::RunSummary)
        );
        let outcome = state.runtime.goal.last_outcome.as_ref().unwrap();
        assert!(outcome.met);
        assert_eq!(outcome.checks, 1);
        let (state, _) = crate::update(state, Msg::Slash(SlashCmd::Goal(None)));
        assert!(
            notes(&state)
                .last()
                .unwrap()
                .starts_with("No goal set. The last goal was met: all tests pass")
        );
    }

    #[test]
    fn impossible_clears_the_goal_and_says_why() {
        let (state, _) = set(fresh_state(), "deploy to prod");
        let (state, _) = reply(state, "I have no credentials.");
        let (state, _) = verdict(state, "IMPOSSIBLE: no deploy credentials exist");
        assert!(matches!(state.turn, TurnState::Idle));
        assert_eq!(state.session.conversation.goal, None);
        assert!(
            notes(&state)
                .iter()
                .any(|n| n.contains("no deploy credentials"))
        );
        assert!(!state.runtime.goal.last_outcome.as_ref().unwrap().met);
    }

    #[test]
    fn turns_without_tools_pause_the_goal() {
        let (mut state, _) = set(fresh_state(), "x");
        for _ in 0..goal::STALL_TURNS - 1 {
            let (s, _) = reply(state, "thinking about it");
            let (s, _) = verdict(s, "NOT_MET: nothing done");
            assert!(matches!(s.turn, TurnState::Generating { .. }));
            state = s;
        }
        let (state, _) = reply(state, "still thinking");
        let (state, cmds) = verdict(state, "NOT_MET: nothing done");
        assert!(matches!(state.turn, TurnState::Idle));
        assert!(!has_cmd(&cmds, |c| matches!(c, Cmd::CallModel { .. })));
        assert_eq!(state.session.conversation.goal.as_deref(), Some("x"));
        assert!(
            notes(&state)
                .iter()
                .any(|n| n.starts_with("Goal paused: 3 goal turns"))
        );
    }

    #[test]
    fn a_turn_with_tools_resets_the_stall_guard() {
        let (mut state, _) = set(fresh_state(), "x");
        for _ in 0..goal::STALL_TURNS + 2 {
            state.runtime.goal.used_tools = true;
            let (s, _) = reply(state, "ran a command");
            let (s, _) = verdict(s, "NOT_MET: keep going");
            assert!(matches!(s.turn, TurnState::Generating { .. }));
            state = s;
        }
    }

    #[test]
    fn the_turn_cap_pauses_until_the_user_writes() {
        let mut state = fresh_state();
        state.settings.goal.max_turns = 1;
        let (mut state, _) = set(state, "x");
        state.runtime.goal.used_tools = true;
        let (mut state, _) = reply(state, "step");
        let (s, _) = verdict(state, "NOT_MET: more");
        assert!(matches!(s.turn, TurnState::Generating { .. }));
        state = s;
        state.runtime.goal.used_tools = true;
        let (state, _) = reply(state, "step");
        let (state, _) = verdict(state, "NOT_MET: more");
        assert!(matches!(state.turn, TurnState::Idle));
        assert!(notes(&state).iter().any(|n| n.contains("[goal] max_turns")));
        // A message from the user restarts the count.
        let (state, _) = crate::update(
            state,
            Msg::SubmitPrompt {
                text: "go on".to_string(),
                attachment_ids: Vec::new(),
            },
        );
        assert_eq!(state.runtime.goal.turns_since_prompt, 0);
    }

    #[test]
    fn a_failed_or_unreadable_check_pauses_the_goal() {
        let (state, _) = set(fresh_state(), "x");
        let (state, _) = reply(state, "step");
        let turn = state.turn.id().unwrap();
        let (state, _) = crate::update(
            state,
            Msg::GoalEvaluated {
                turn,
                reply: Err("no reply in 120s".to_string()),
            },
        );
        assert!(matches!(state.turn, TurnState::Idle));
        assert_eq!(state.session.conversation.goal.as_deref(), Some("x"));
        assert!(notes(&state).iter().any(|n| n.contains("no reply in 120s")));

        let (state, _) = crate::update(
            state,
            Msg::SubmitPrompt {
                text: "again".to_string(),
                attachment_ids: Vec::new(),
            },
        );
        let (state, _) = reply(state, "step");
        let (state, _) = verdict(state, "I think it is probably fine");
        assert!(matches!(state.turn, TurnState::Idle));
        assert!(notes(&state).iter().any(|n| n.contains("gave no verdict")));
    }

    #[test]
    fn esc_during_a_check_keeps_the_goal_and_drops_the_late_reply() {
        let (state, _) = set(fresh_state(), "x");
        let (state, _) = reply(state, "step");
        let turn = state.turn.id().unwrap();
        let (state, cmds) = crate::update(state, Msg::CancelTurn);
        assert!(has_cmd(
            &cmds,
            |c| matches!(c, Cmd::CancelScope(t) if *t == turn)
        ));
        let (state, _) = crate::update(
            state,
            Msg::GoalEvaluated {
                turn,
                reply: Ok(goal::GoalReply {
                    text: "MET: yes".to_string(),
                    reasoning: None,
                    usage: None,
                }),
            },
        );
        assert!(matches!(state.turn, TurnState::Cancelling { .. }));
        let (state, _) = crate::update(state, Msg::TurnCancelled(turn));
        assert!(matches!(state.turn, TurnState::Idle));
        assert_eq!(state.session.conversation.goal.as_deref(), Some("x"));
        assert!(notes(&state).iter().any(|n| n.starts_with("Goal paused.")));
    }

    #[test]
    fn a_queued_message_goes_before_the_next_goal_turn() {
        let (state, _) = set(fresh_state(), "x");
        let (state, _) = reply(state, "step");
        let (state, _) = crate::update(
            state,
            Msg::SubmitPrompt {
                text: "use the other API".to_string(),
                attachment_ids: Vec::new(),
            },
        );
        assert_eq!(state.ui.queued_messages.len(), 1);
        let (state, _) = verdict(state, "NOT_MET: more");
        assert!(matches!(state.turn, TurnState::Generating { .. }));
        let last_user = state
            .session
            .messages()
            .iter()
            .rev()
            .find(|m| m.role == MessageRole::User)
            .unwrap();
        assert_eq!(last_user.content, "use the other API");
        assert_eq!(state.session.conversation.goal.as_deref(), Some("x"));
    }

    #[test]
    fn background_agents_defer_the_check() {
        let (mut state, _) = set(fresh_state(), "x");
        state
            .runtime
            .background_agents
            .push(crate::BackgroundAgent {
                agent_id: "a1".to_string(),
                description: "search".to_string(),
                started: std::time::SystemTime::UNIX_EPOCH,
                activity: String::new(),
                tokens: 0,
            });
        let (state, cmds) = reply(state, "waiting on the agent");
        assert!(matches!(state.turn, TurnState::Idle));
        assert!(!has_cmd(&cmds, |c| matches!(c, Cmd::EvaluateGoal { .. })));
        assert!(
            notes(&state)
                .iter()
                .any(|n| n == "The goal check waits for 1 background agent to finish.")
        );
    }

    #[test]
    fn clear_and_status() {
        let (state, _) = crate::update(fresh_state(), Msg::Slash(SlashCmd::Goal(None)));
        assert!(notes(&state)[0].starts_with("No goal set. Usage: /goal <condition>"));
        let (state, _) =
            crate::update(state, Msg::Slash(SlashCmd::Goal(Some("clear".to_string()))));
        assert_eq!(notes(&state).last().unwrap(), "No goal set.");

        let (state, _) = set(state, "lint is clean");
        let (state, _) = reply(state, "step");
        let (state, _) = crate::update(state, Msg::Slash(SlashCmd::Goal(None)));
        let status = notes(&state).last().unwrap().clone();
        assert!(
            status.starts_with("Goal: lint is clean\nRunning for"),
            "{status}"
        );
        let (state, _) = crate::update(state, Msg::Slash(SlashCmd::Goal(Some("stop".to_string()))));
        assert_eq!(state.session.conversation.goal, None);
        assert_eq!(notes(&state).last().unwrap(), "Goal cleared: lint is clean");
        // The check that was running finds no goal and ends the run.
        let (state, cmds) = verdict(state, "NOT_MET: more");
        assert!(matches!(state.turn, TurnState::Idle));
        assert!(!has_cmd(&cmds, |c| matches!(c, Cmd::CallModel { .. })));
    }

    #[test]
    fn an_overlong_goal_is_refused() {
        let long = "x".repeat(goal::MAX_CONDITION_CHARS + 1);
        let (state, cmds) = set(fresh_state(), &long);
        assert_eq!(state.session.conversation.goal, None);
        assert!(!has_cmd(&cmds, |c| matches!(c, Cmd::CallModel { .. })));
    }

    #[test]
    fn the_goal_survives_a_save_and_clear_removes_it() {
        let (state, _) = set(fresh_state(), "x");
        let snapshot = state.session.snapshot_conversation();
        assert_eq!(snapshot.goal.as_deref(), Some("x"));
        let json = serde_json::to_string(&snapshot).unwrap();
        let back: crate::ConversationHistory = serde_json::from_str(&json).unwrap();
        assert_eq!(back.goal.as_deref(), Some("x"));
        // A save without a goal does not write the key at all.
        let json = serde_json::to_string(&fresh_state().session.snapshot_conversation()).unwrap();
        assert!(!json.contains("\"goal\""));
    }
}
