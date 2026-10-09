//! `/btw` in the reducer: ask, settle, and drive the side-question pane.
//!
//! The state and the request live in `crate::side_question`; this module only
//! maps commands, messages and keys onto them.

use crate::cmd::Cmd;
use crate::msg::{KeyCode, KeyMods};
use crate::side_question::{SideOutcome, side_question_request};
use crate::state::State;

/// `/btw <question>` asks; a bare `/btw` reopens the pane on the newest
/// exchange. Runs the same whether or not a turn is in flight: the side call
/// is detached from it.
pub fn handle_btw(state: &mut State, cmds: &mut Vec<Cmd>, question: Option<String>) {
    match question {
        Some(question) => {
            // Build first: the request must not replay the new question.
            let request = side_question_request(state, &question);
            let id = state.side_questions.ask(question);
            cmds.push(Cmd::AskSideQuestion { id, request });
        },
        None => {
            if !state.side_questions.reopen() {
                super::push_system(
                    state,
                    cmds,
                    "Usage: /btw <question> (asks a side question; /btw alone reopens the last answer)",
                );
            }
        },
    }
}

/// A side question's call ended. When the pane is closed, a toast says the
/// answer is ready; it never lands in the transcript.
pub fn handle_side_question_finished(state: &mut State, id: u64, outcome: SideOutcome) {
    if state.side_questions.finish(id, outcome) && state.side_questions.view.is_none() {
        state.ui.toast = Some((
            "Side answer ready. Type /btw to see it.".to_string(),
            state.now + crate::state::TOAST_TTL,
        ));
    }
}

/// Keys while the pane has focus. Every key is the pane's; none reaches the
/// composer or cancels the main turn.
pub fn handle_side_question_key(
    state: &mut State,
    cmds: &mut Vec<Cmd>,
    code: KeyCode,
    mods: KeyMods,
) {
    let side = &mut state.side_questions;
    match code {
        KeyCode::Escape | KeyCode::Enter | KeyCode::Char(' ') => side.dismiss(),
        KeyCode::Up => side.scroll(-1),
        KeyCode::Down => side.scroll(1),
        KeyCode::Left | KeyCode::BackTab | KeyCode::Char('[') => side.older(),
        KeyCode::Right | KeyCode::Tab | KeyCode::Char(']') => side.newer(),
        KeyCode::Char('c') if !mods.ctrl => {
            if let Some(ex) = side.viewed()
                && !ex.answer.is_empty()
            {
                cmds.push(Cmd::CopyToClipboard(ex.answer.clone()));
            }
        },
        KeyCode::Char('x') if !mods.ctrl => side.clear_earlier(),
        _ => {},
    }
}

#[cfg(test)]
mod tests {
    use crate::Config;
    use crate::cmd::Cmd;
    use crate::msg::{Key, KeyCode, KeyMods, Msg, SlashCmd};
    use crate::side_question::{SideOutcome, SideStatus};
    use crate::state::{Focus, State, TurnState};
    use crate::{start_generating, update};
    use mermaid_model::ids::TurnId;
    use mermaid_model::models::{ChatMessage, MessageRole};

    fn fresh_state() -> State {
        State::new(
            Config::default(),
            std::path::PathBuf::from("/tmp/project"),
            "ollama/test".to_string(),
            chrono::Local::now(),
            std::path::PathBuf::from("/tmp"),
        )
    }

    fn key(code: KeyCode) -> Msg {
        Msg::Key(Key {
            code,
            modifiers: KeyMods::NONE,
        })
    }

    fn ask(state: State, question: &str) -> (State, Vec<Cmd>) {
        update(state, Msg::Slash(SlashCmd::Btw(Some(question.to_string()))))
    }

    fn side_request(cmds: &[Cmd]) -> (u64, &crate::ChatRequest) {
        cmds.iter()
            .find_map(|c| match c {
                Cmd::AskSideQuestion { id, request } => Some((*id, request)),
                _ => None,
            })
            .expect("a side question dispatches AskSideQuestion")
    }

    fn answer(state: State, id: u64, text: &str) -> State {
        let (state, _) = update(
            state,
            Msg::SideQuestionText {
                id,
                chunk: text.to_string(),
            },
        );
        let (state, cmds) = update(
            state,
            Msg::SideQuestionFinished {
                id,
                outcome: SideOutcome::Done { tried_tools: false },
            },
        );
        assert!(cmds.is_empty(), "settling a side answer emits no effect");
        state
    }

    #[test]
    fn btw_while_busy_leaves_the_main_turn_and_conversation_alone() {
        let mut state = fresh_state();
        state
            .session
            .append(ChatMessage::user("refactor the parser"), state.now);
        state.turn = start_generating(TurnId(7), std::time::SystemTime::now());
        let before = state.session.messages().len();

        let (state, cmds) = ask(state, "what file was that?");
        assert!(
            matches!(state.turn, TurnState::Generating { id: TurnId(7), .. }),
            "the main turn keeps running"
        );
        assert!(
            !cmds
                .iter()
                .any(|c| matches!(c, Cmd::CancelScope(_) | Cmd::CallModel { .. })),
            "nothing touches the main turn"
        );
        assert!(
            !cmds
                .iter()
                .any(|c| matches!(c, Cmd::SaveConversation { .. })),
            "nothing is persisted"
        );
        let (id, request) = side_request(&cmds);
        // The side call sees the conversation, then the question.
        assert_eq!(request.messages[0].content, "refactor the parser");
        let last = request.messages.last().unwrap();
        assert_eq!(last.role, MessageRole::User);
        assert!(last.content.contains("what file was that?"));
        assert!(request.suppress_auto_compact);

        let state = answer(state, id, "src/parser.rs");
        assert_eq!(state.session.messages().len(), before, "never in history");
        assert_eq!(state.focus(), Focus::SideQuestion);
        let shown = state.side_questions.viewed().unwrap();
        assert_eq!(shown.answer, "src/parser.rs");
        assert_eq!(shown.status, SideStatus::Done);
    }

    #[test]
    fn later_questions_replay_earlier_answers_but_never_the_main_history() {
        let (state, cmds) = ask(fresh_state(), "first?");
        let (id, _) = side_request(&cmds);
        let state = answer(state, id, "one");
        let (state, cmds) = ask(state, "second?");
        let (_, request) = side_request(&cmds);
        let contents: Vec<&str> = request
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect();
        assert_eq!(contents.len(), 3, "{contents:?}");
        assert!(contents[0].contains("first?"));
        assert_eq!(contents[1], "one");
        assert!(contents[2].contains("second?"));
        assert!(state.session.messages().is_empty());
    }

    #[test]
    fn esc_closes_the_pane_without_cancelling_the_turn() {
        let mut state = fresh_state();
        state.turn = start_generating(TurnId(3), std::time::SystemTime::now());
        let (state, _) = ask(state, "q");
        let (state, cmds) = update(state, key(KeyCode::Escape));
        assert!(cmds.is_empty(), "Esc on the pane cancels nothing: {cmds:?}");
        assert!(matches!(state.turn, TurnState::Generating { .. }));
        assert_eq!(state.focus(), Focus::Composer);
    }

    #[test]
    fn an_answer_landing_after_dismiss_raises_a_toast_and_btw_reopens_it() {
        let (state, cmds) = ask(fresh_state(), "q");
        let (id, _) = side_request(&cmds);
        let (state, _) = update(state, key(KeyCode::Escape));
        let state = answer(state, id, "a");
        assert!(state.ui.toast.is_some());
        assert!(state.session.messages().is_empty());

        let (state, cmds) = update(state, Msg::Slash(SlashCmd::Btw(None)));
        assert!(cmds.is_empty());
        assert_eq!(state.side_questions.viewed().unwrap().answer, "a");
    }

    #[test]
    fn bare_btw_with_no_thread_prints_usage() {
        let (state, _) = update(fresh_state(), Msg::Slash(SlashCmd::Btw(None)));
        assert!(state.side_questions.view.is_none());
        assert!(
            state
                .session
                .messages()
                .last()
                .is_some_and(|m| m.content.starts_with("Usage: /btw"))
        );
    }

    #[test]
    fn pane_keys_copy_step_and_clear() {
        let (state, cmds) = ask(fresh_state(), "a?");
        let (id, _) = side_request(&cmds);
        let state = answer(state, id, "A");
        let (state, cmds) = ask(state, "b?");
        let (id, _) = side_request(&cmds);
        let state = answer(state, id, "B");

        let (state, cmds) = update(state, key(KeyCode::Char('c')));
        assert!(matches!(&cmds[..], [Cmd::CopyToClipboard(t)] if t == "B"));
        let (state, _) = update(state, key(KeyCode::Left));
        assert_eq!(state.side_questions.viewed().unwrap().answer, "A");
        let (state, _) = update(state, key(KeyCode::Char('x')));
        assert_eq!(state.side_questions.exchanges.len(), 1);
        // Typed letters belong to the pane, not the composer.
        let (state, _) = update(state, key(KeyCode::Char('q')));
        assert!(state.ui.input_buffer.is_empty());
    }

    #[test]
    fn an_approval_outranks_the_pane() {
        let (mut state, _) = ask(fresh_state(), "q");
        state
            .pending_approval
            .push_back(crate::state::PendingApproval {
                turn: TurnId(1),
                call_id: mermaid_model::ids::ToolCallId(1),
                tool: "execute_command".to_string(),
                risk: "medium".to_string(),
                kind: crate::state::ApprovalKind::Shell,
                prompt: "ls".to_string(),
                allowlist_scope: String::new(),
                selected_option: 0,
            });
        assert_eq!(state.focus(), Focus::ApprovalModal);
    }

    #[test]
    fn clear_forgets_the_side_thread() {
        let (state, _) = ask(fresh_state(), "q");
        let (state, _) = update(state, Msg::Slash(SlashCmd::Clear));
        let (state, _) = update(state, Msg::ConfirmAccepted);
        assert!(state.side_questions.exchanges.is_empty());
        assert!(state.side_questions.view.is_none());
    }
}
