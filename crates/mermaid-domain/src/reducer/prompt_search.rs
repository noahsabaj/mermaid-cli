//! Ctrl+R: search the prompts sent earlier, in this session and in the
//! project's saved sessions, and put the chosen one in the composer.
//!
//! The picker opens at once on this session's history; the saved sessions'
//! prompts arrive later (`Query::ListRecentPrompts`) and are appended behind
//! them, so a slow disk never delays the keystroke.

use crate::cmd::Cmd;
use crate::msg::KeyCode;
use crate::picker::{PickerStep, picker_step};
use crate::query::Query;
use crate::state::{State, UiMode};

/// How many saved sessions and prompts the search reads.
pub const PROMPT_SEARCH_MAX_SESSIONS: usize = 50;
pub const PROMPT_SEARCH_MAX_PROMPTS: usize = 1000;

/// Open the search over this session's prompts (newest first) and ask the
/// effect layer for the saved sessions' prompts.
pub fn open_prompt_search(state: &mut State, cmds: &mut Vec<Cmd>) {
    let mut candidates = Vec::new();
    for prompt in state.session.conversation.input_history.iter().rev() {
        if !candidates.contains(prompt) {
            candidates.push(prompt.clone());
        }
    }
    state.ui.mode = UiMode::PromptSearch {
        candidates,
        query: String::new(),
        cursor: 0,
        loading: true,
    };
    cmds.push(Cmd::Query(Query::ListRecentPrompts {
        max_sessions: PROMPT_SEARCH_MAX_SESSIONS,
        max_prompts: PROMPT_SEARCH_MAX_PROMPTS,
    }));
}

/// The saved sessions' prompts landed: append the ones not already listed.
/// Dropped when the user closed the search first.
pub fn merge_recent_prompts(state: &mut State, prompts: Vec<String>) {
    if let UiMode::PromptSearch {
        candidates,
        loading,
        ..
    } = &mut state.ui.mode
    {
        for prompt in prompts {
            if !candidates.contains(&prompt) {
                candidates.push(prompt);
            }
        }
        *loading = false;
    }
}

/// The candidates that match `query`: every whitespace-separated word must
/// appear, ignoring case. An empty query matches everything.
#[must_use]
pub fn filter_prompts<'a>(candidates: &'a [String], query: &str) -> Vec<&'a String> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    candidates
        .iter()
        .filter(|prompt| {
            let prompt = prompt.to_lowercase();
            words.iter().all(|word| prompt.contains(word.as_str()))
        })
        .collect()
}

/// Ctrl+R while the search is open: the next older match, like a shell.
pub fn prompt_search_next(state: &mut State) {
    if let UiMode::PromptSearch {
        candidates,
        query,
        cursor,
        ..
    } = &mut state.ui.mode
    {
        let len = filter_prompts(candidates, query).len();
        if *cursor + 1 < len {
            *cursor += 1;
        }
    }
}

/// Keys while the search is open: arrows move, typing narrows, Enter puts
/// the highlighted prompt in the composer (it is not sent), Esc closes.
pub fn handle_prompt_search_key(state: &mut State, code: KeyCode) {
    let UiMode::PromptSearch {
        ref candidates,
        ref mut query,
        ref mut cursor,
        ..
    } = state.ui.mode
    else {
        return;
    };
    let filtered_len = filter_prompts(candidates, query).len();
    match picker_step(code, cursor, filtered_len) {
        PickerStep::Confirm(row) => {
            let chosen = filter_prompts(candidates, query)
                .get(row)
                .map(|prompt| (*prompt).clone());
            state.ui.mode = UiMode::EditingInput;
            if let Some(prompt) = chosen {
                state.ui.input_buffer = prompt;
                state.ui.input_cursor = state.ui.input_buffer.len();
                state.ui.input_history_cursor = None;
                state.ui.history_draft.clear();
                state.ui.palette_cursor = None;
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
                *cursor = 0;
            },
            _ => {},
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn every_query_word_must_match_ignoring_case() {
        let list = strings(&["Fix the Parser", "fix tests", "parse logs"]);
        let hits = filter_prompts(&list, "fix PARS");
        assert_eq!(hits, vec![&list[0]]);
        assert_eq!(filter_prompts(&list, "").len(), 3);
        assert!(filter_prompts(&list, "nothing").is_empty());
    }
}
